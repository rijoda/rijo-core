//! TUI — Ratatui terminal UI for the agent loop
//!
//! Layout: chat history (scrollable) + status line + input box.
//! Keys: Enter=send, Ctrl+C=quit when idle/cancel while running, Up/Down history or scroll, PgUp/PgDn scroll.

use ratatui::{
    crossterm::{
        event::{self, Event as CEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use std::io::stdout;
use std::sync::Arc;
use tokio::sync::mpsc;

// The UI contract lives in `rijo_core::ui_events`; the TUI is one consumer of it.
use crate::clipboard::ClipboardRead;
use rijo_core::ui_events::UiEvent;
pub use rijo_core::ui_events::{
    AskOption, AskQuestion, ChatEntry, ConfirmTx, EntryRole, HistoryHit, SearchMode,
};

/// Everything the terminal frontend handles: its own input plus the core's events.
///
/// The core never produces the first four variants (it has no terminal), and the
/// terminal never produces [`TuiEvent::Core`] itself — a forwarding task wraps
/// every [`UiEvent`] that arrives from the engine.
#[derive(Debug, Clone)]
pub enum TuiEvent {
    Key(KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Paste(String),
    Clipboard(ClipboardRead),
    Core(UiEvent),
}

impl From<UiEvent> for TuiEvent {
    fn from(event: UiEvent) -> Self {
        TuiEvent::Core(event)
    }
}

#[derive(Debug, Clone)]
struct EntryRenderCache {
    signature: u64,
    lines: Vec<Line<'static>>,
}

fn entry_render_signature(entry: &ChatEntry, index: usize, width: u16, verbose: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    entry.role.hash(&mut hasher);
    entry.text.hash(&mut hasher);
    entry.expanded.hash(&mut hasher);
    if verbose {
        entry.detail.hash(&mut hasher);
    }
    index.hash(&mut hasher);
    width.hash(&mut hasher);
    verbose.hash(&mut hasher);
    hasher.finish()
}

fn selection_content_signature(app: &App, width: u16) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    width.hash(&mut hasher);
    app.render_cache.len().hash(&mut hasher);
    for entry in &app.render_cache {
        entry
            .as_ref()
            .map(|entry| entry.signature)
            .hash(&mut hasher);
    }
    app.stream_buf.as_ref().map(String::len).hash(&mut hasher);
    hasher.finish()
}

fn collect_visible_chat_lines(
    cache: &[Option<EntryRenderCache>],
    stream: &[Line<'static>],
    start: usize,
    count: usize,
) -> Vec<Line<'static>> {
    let end = start.saturating_add(count);
    let mut cursor = 0usize;
    let mut visible = Vec::with_capacity(count);
    for lines in cache
        .iter()
        .filter_map(Option::as_ref)
        .map(|entry| entry.lines.as_slice())
        .chain(std::iter::once(stream))
    {
        let line_end = cursor.saturating_add(lines.len());
        let from = start.saturating_sub(cursor).min(lines.len());
        let to = end.saturating_sub(cursor).min(lines.len());
        if from < to {
            visible.extend(lines[from..to].iter().cloned());
        }
        cursor = line_end;
        if cursor >= end {
            break;
        }
    }
    visible
}

#[derive(Debug, Clone)]
struct ToolCatalogEntry {
    name: String,
    toolset: String,
    description: String,
    schema: serde_json::Value,
}

#[derive(Debug, Clone)]
struct OperationsItem {
    title: String,
    detail: String,
    prompt: String,
}

#[derive(Debug, Clone, Default)]
struct HistoryView {
    kind: SearchMode,
    query: String,
    selected: usize,
    results: Vec<HistoryHit>,
    searching: bool,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct MemoryItem {
    target: String,
    content: String,
}

fn parse_history_results(raw: &str) -> Result<Vec<HistoryHit>, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid session search result: {e}"))?;
    if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
        return Err(error.to_string());
    }
    let Some(results) = value.get("results").and_then(|v| v.as_array()) else {
        return Err("session search returned no results field".to_string());
    };
    Ok(results
        .iter()
        .filter_map(|result| {
            let session_id = result.get("session_id")?.as_str()?.to_string();
            let role = result.get("role")?.as_str()?.to_string();
            let timestamp = result.get("ts")?.as_str()?.to_string();
            let content = result.get("content")?.as_str()?;
            Some(HistoryHit {
                session_id,
                role,
                timestamp,
                excerpt: content.chars().take(220).collect(),
            })
        })
        .collect())
}

fn parse_brain_results(raw: &str) -> Result<Vec<HistoryHit>, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid brain search result: {e}"))?;
    if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
        return Err(error.to_string());
    }
    if let Some(note) = value.get("note").and_then(|v| v.as_str()) {
        if value.get("hits").and_then(|v| v.as_array()).is_none() {
            return Err(note.to_string());
        }
    }
    let Some(results) = value.get("hits").and_then(|v| v.as_array()) else {
        return Err("brain search returned no hits field".to_string());
    };
    if results.is_empty() {
        if let Some(note) = value.get("note").and_then(|v| v.as_str()) {
            return Err(note.to_string());
        }
    }
    Ok(results
        .iter()
        .filter_map(|result| {
            let source = result.get("source")?.as_str()?.to_string();
            let kind = result
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("source")
                .to_string();
            let score = result
                .get("score")
                .and_then(|v| v.as_f64())
                .map(|score| format!("score {score:.3}"))
                .unwrap_or_else(|| "score unavailable".to_string());
            let excerpt = result.get("excerpt").and_then(|v| v.as_str()).unwrap_or("");
            Some(HistoryHit {
                session_id: source,
                role: kind,
                timestamp: score,
                excerpt: excerpt.chars().take(220).collect(),
            })
        })
        .collect())
}

fn history_prompt(hit: &HistoryHit) -> String {
    format!(
        "Use this past exchange as context from session {} ({}):\n{}\n\nContinue with: ",
        hit.session_id, hit.role, hit.excerpt
    )
}

fn search_hit_prompt(kind: SearchMode, hit: &HistoryHit) -> String {
    match kind {
        SearchMode::History => history_prompt(hit),
        SearchMode::Brain => format!(
            "Use this indexed workspace result as context ({} · {}):\n{}\n\nContinue with: ",
            hit.session_id, hit.timestamp, hit.excerpt
        ),
    }
}

fn build_tool_catalog(registry: &rijo_core::tools::Registry) -> Vec<ToolCatalogEntry> {
    let mut entries: Vec<_> = registry
        .get_names()
        .into_iter()
        .filter_map(|name| {
            let meta = registry.get_meta(name)?;
            let schema = registry.get_schema(name)?.raw().clone();
            Some(ToolCatalogEntry {
                name: name.to_string(),
                toolset: meta.toolset.to_string(),
                description: meta.description.to_string(),
                schema,
            })
        })
        .collect();
    entries.sort_by(|a, b| (&a.toolset, &a.name).cmp(&(&b.toolset, &b.name)));
    entries
}

fn build_runtime_lines(
    config: &rijo_core::config::Config,
    tool_count: usize,
    box_enabled: bool,
    router_loaded: bool,
) -> Vec<String> {
    let router = if !config.router.enabled {
        "off"
    } else if router_loaded {
        "loaded"
    } else {
        "not loaded · full toolset fallback"
    };
    let brain = if config.memory.enabled {
        "enabled · vector query attempted per turn; full-text fallback is reported"
    } else {
        "disabled"
    };
    let enabled_mcp: Vec<&str> = config
        .mcp
        .servers
        .iter()
        .filter(|server| server.enabled)
        .map(|server| server.name.as_str())
        .collect();
    let mcp = if enabled_mcp.is_empty() {
        "none configured".to_string()
    } else {
        format!(
            "{} configured · connectivity not checked ({})",
            enabled_mcp.len(),
            enabled_mcp.join(", ")
        )
    };
    let memory_bytes = ["MEMORY.md", "USER.md"]
        .iter()
        .filter_map(|name| {
            let metadata =
                std::fs::symlink_metadata(config.data_dir.join("memories").join(name)).ok()?;
            metadata.file_type().is_file().then_some(metadata.len())
        })
        .sum::<u64>();
    let memory = if memory_bytes == 0 {
        "none saved".to_string()
    } else {
        format!("{memory_bytes} bytes stored · contents hidden")
    };
    vec![
        format!(
            "LLM model: {} · API key {}",
            config.llm.model,
            if config.llm.api_key.is_empty() {
                "missing"
            } else {
                "configured"
            }
        ),
        format!(
            "Tools: {tool_count} registered · Box {}",
            if box_enabled { "enabled" } else { "not configured" }
        ),
        format!("Router: {router}"),
        format!("Brain: {brain}"),
        format!("Memory: {memory}"),
        format!("Browser: {}", config.browser_backend),
        format!(
            "Sandbox: {} · network {}",
            config.sandbox.backend,
            if config.sandbox.network { "on" } else { "off" }
        ),
        format!("MCP: {mcp}"),
        "CLI only: --index · --sync-* · --download-models · --bench · --schema-eval · --dump-labels · mcp serve".to_string(),
    ]
}

fn filtered_tool_indices(catalog: &[ToolCatalogEntry], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    catalog
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            query.is_empty()
                || entry.name.to_lowercase().contains(&query)
                || entry.toolset.to_lowercase().contains(&query)
                || entry.description.to_lowercase().contains(&query)
                || entry.schema.to_string().to_lowercase().contains(&query)
        })
        .map(|(i, _)| i)
        .collect()
}

fn filtered_skill_indices(
    skills: &[rijo_core::tools::skills_tool::SkillEntry],
    query: &str,
) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    skills
        .iter()
        .enumerate()
        .filter(|(_, skill)| {
            query.is_empty()
                || skill.name.to_lowercase().contains(&query)
                || skill.category.to_lowercase().contains(&query)
                || skill.description.to_lowercase().contains(&query)
        })
        .map(|(i, _)| i)
        .collect()
}

fn filtered_activity_indices(archive: &[String], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    archive
        .iter()
        .enumerate()
        .filter(|(_, item)| query.is_empty() || item.to_lowercase().contains(&query))
        .map(|(i, _)| i)
        .collect()
}

fn palette_command_needs_argument(command: &str) -> bool {
    matches!(command, "/attach" | "/thinking")
}

fn allows_text_entry(modifiers: KeyModifiers) -> bool {
    modifiers.is_empty() || modifiers == KeyModifiers::SHIFT
}

fn insert_palette_command_prefix(app: &mut App, command: &str) {
    let prefix = format!("{command} ");
    app.input.insert_str(0, &prefix);
    app.cursor = prefix.len();
    app.history_idx = None;
}

fn insert_prompt_template(app: &mut App, prompt: &str) {
    let prefix = if app.input.is_empty() {
        prompt.to_string()
    } else {
        format!("{prompt}\n")
    };
    app.input.insert_str(0, &prefix);
    app.cursor = prefix.len();
    app.history_idx = None;
}

fn insert_operation_prompt(app: &mut App, prompt: &str) {
    insert_prompt_template(app, prompt);
    app.operations_selected = None;
}

struct App {
    entries: Vec<ChatEntry>,
    render_cache: Vec<Option<EntryRenderCache>>,
    selection_signature: Option<u64>,
    stream_buf: Option<String>,
    /// True after StreamDone finalised stream_buf into an entry.
    /// Used to skip duplicate TuiEvent::Entry from the async task.
    stream_finalized: bool,
    input: String,
    cursor: usize,
    /// shared conversation history across turns (context-window accumulator);
    /// grows with every user/assistant exchange within the session
    conversation_history: Vec<rijo_core::llm::ChatMessage>,
    /// Edge auto-scroll state while dragging a selection: Some(true)=up, Some(false)=down
    autoscroll: Option<bool>,
    /// live turn phase for the status line: What is rijo doing RIGHT NOW
    phase: String,
    /// wall-clock start of the running turn (turn duration display)
    turn_started: Option<std::time::Instant>,
    /// Thinking effort tier: min|med|max — /thinking switch, default med
    thinking_effort: String,
    /// wall-clock start of the CURRENT running tool (⚙ phase ticker)
    tool_started: Option<std::time::Instant>,
    /// Ctrl+O toggle: show full tool/reasoning detail
    verbose: bool,
    /// accumulating reasoning text for the live thinking entry
    reasoning_buf: String,
    /// index of the live thinking entry in entries (removed/reset on finalize)
    reasoning_idx: Option<usize>,
    /// running tools: call-key -> entry index, for in-place ToolCall→ToolDone update
    open_tools: std::collections::HashMap<String, usize>,
    input_history: Vec<String>,
    history_idx: Option<usize>,
    /// absolute top line of viewport; None = follow bottom
    /// lines the viewport bottom sits above the content bottom;
    /// None = follow bottom (auto-scroll). Stable while streaming.
    bottom_anchor: Option<u16>,
    /// spinner animation frame (unbounded; the glyph helper takes the modulo)
    spinner_frame: usize,
    /// (mtime, size) of todos.json at the last read — the tick re-reads the file
    /// only when it changed, so the strip stays live even without an event.
    todos_sig: Option<(std::time::SystemTime, u64)>,
    /// Tool calls since the todo list last CHANGED. A list the model keeps working
    /// past is stale — the strip says so instead of pretending to be current, and the
    /// model gets a nudge in the tool result (see executor::todo_note).
    calls_since_todos: u32,
    /// scroll offset used when painting the chat (kept for mouse mapping)
    last_chat_area: Option<(u16, u16, u16, u16)>, // x, y, w, h
    last_chat_scroll: u16,
    /// Screen rows of the status bars, written by `draw_frame`, so a left click
    /// can toggle the matching view: activity = Ctrl+O details, todo = Ctrl+T.
    last_row_activity: Option<u16>,
    last_row_todo: Option<u16>,
    /// transient toast (text, ticks remaining); None = hidden
    toast: Option<(String, u32)>,
    /// @file fuzzy picker overlay: active (query, selected index, files)
    picker: Option<(String, usize, Vec<String>)>,
    /// command palette overlay (ctrl+p): query, selected index, all commands
    palette: Option<(String, usize, Vec<&'static str>)>,
    /// Searchable, read-only directory of the live registered tool contracts.
    tool_catalog: Vec<ToolCatalogEntry>,
    tool_explorer: Option<(String, usize)>,
    skills_catalog: Vec<rijo_core::tools::skills_tool::SkillEntry>,
    skills_view: Option<(String, usize)>,
    runtime_lines: Vec<String>,
    runtime_scroll: Option<u16>,
    operations_items: Vec<OperationsItem>,
    operations_selected: Option<usize>,
    history_view: Option<HistoryView>,
    state: Option<Arc<rijo_core::AppState>>,
    memory_items: Vec<MemoryItem>,
    memory_selected: Option<usize>,
    archived_activity: Vec<String>,
    compacted_activity_count: usize,
    activity_view: Option<(String, usize)>,
    /// Active confirm dialog (tx, message). When Some, the TUI shows an overlay
    /// and blocks normal input until y/n is pressed.
    confirm: Option<(Arc<ConfirmTx>, String)>,
    select_panel: tui_panel_select::SelectablePanel,
    selected_prompt: i32,
    running: bool,
    /// Numbers of the last turn (from ProgressEvent::TurnStats) — otherwise
    /// the measurements would only be visible in the JSONL file, not in the UI.
    last_stats: Option<TurnStatsView>,
    /// Context window of the configured model (0 = unknown) — set once at
    /// startup so the gauge can show a percentage even before the first turn.
    context_window: u64,
    /// Estimated request size of the running turn (characters), from
    /// ProgressEvent::ContextEstimate: the provider reports real tokens only with
    /// the answer, so during a turn the gauge shows an estimate (marked "~").
    live_ctx_chars: usize,
    live_ctx_messages: usize,
    /// Session totals, accumulated from the per-turn stats: what the whole
    /// session has cost so far (a single turn says little about that).
    session_turns: u32,
    /// Todo-list snapshot (read from `todo_list`'s file), shown as a strip above
    /// the input so the current task is always visible.
    todos: Vec<TodoView>,
    /// Ctrl+T: expand the one-line strip into the full list.
    todos_expanded: bool,
    /// An open question overlay. While Some, all keys go to it and the turn that
    /// asked is blocked waiting for the answers.
    ask: Option<AskState>,
    /// Files attached to the NEXT message (`/attach`). Images go through the
    /// model's vision path, text files are inlined when the turn starts.
    attachments: Vec<String>,
    session_prompt_tokens: u64,
    session_completion_tokens: u64,
}

/// Compact view of the turn measurement for the status line.
#[derive(Clone, Debug)]
struct TurnStatsView {
    iterations: u32,
    llm_ms: u64,
    tool_ms: u64,
    total_ms: u64,
    tool_calls: u32,
    prompt_tokens: u32,
    /// Prompt tokens of the last request — the real context fill. `prompt_tokens`
    /// is the sum over all iterations (billed volume).
    last_prompt_tokens: u64,
    completion_tokens: u32,
    cache_hit: u32,
    cache_miss: u32,
    /// Tool calls that failed — the only way to see a failing turn at a glance.
    failures: u32,
    /// Brain hits this turn (0 = the index contributed nothing).
    brain_hits: usize,
    /// Time to first token in ms (0 = the provider streamed nothing).
    ttft_ms: u64,
    /// Context window of the model (0 = unknown, then no percentage).
    context_window: u64,
}

/// A duration in ms as "412ms" / "1.4s". Shared by the compact and full rows.
fn fmt_dur(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

/// A token count as "33" / "4.4k".
fn fmt_tok(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

impl TurnStatsView {
    /// Provider cache ratio, when it reported one (silent provider → `None`, no
    /// invented "Cache 0%").
    fn cache_pct(&self) -> Option<u32> {
        let seen = self.cache_hit + self.cache_miss;
        (seen > 0).then(|| (self.cache_hit as f64 / seen as f64 * 100.0).round() as u32)
    }

    /// The short form shown by default. Context fill and TTFT are their own
    /// segments (see `data_segments`); here live the turn duration, the cache
    /// ratio and any failures. Everything else — the LLM/tool split, iteration
    /// and tool counts, throughput, brain hits, session totals — is one Ctrl+O
    /// away. Showing all eleven values at once buried the few worth a glance.
    fn compact_line(&self) -> String {
        let mut s = format!("turn {}", fmt_dur(self.total_ms));
        if let Some(pct) = self.cache_pct() {
            s.push_str(&format!(" · cache {pct}% {}", gauge(pct)));
        }
        if self.failures > 0 {
            s.push_str(&format!(" · {} failed", self.failures));
        }
        s
    }

    /// The full breakdown (Ctrl+O).
    /// "turn 1.4s · LLM 1.1s · 2× · 5 Tool · →33 tok · 30 tok/s · cache 80% · brain 4"
    fn line(&self) -> String {
        let mut s = format!("turn {}", fmt_dur(self.total_ms));
        if self.llm_ms > 0 {
            s.push_str(&format!(" · LLM {}", fmt_dur(self.llm_ms)));
        }
        if self.tool_ms > 0 {
            s.push_str(&format!(" · Tools {}", fmt_dur(self.tool_ms)));
        }
        if self.iterations > 1 {
            // keep it short: the status line shares the width with everything else
            s.push_str(&format!(" · {}×", self.iterations));
        }
        if self.tool_calls > 0 {
            s.push_str(&format!(" · {} Tool", self.tool_calls));
        }
        // Only the completion side: the prompt size is the context gauge now
        // (showing it twice wasted width on exactly the narrow terminals where
        // something has to go).
        if self.completion_tokens > 0 {
            s.push_str(&format!(" · →{} tok", fmt_tok(self.completion_tokens)));
        }
        // Real throughput of the turn: provider-reported completion tokens over
        // the measured LLM time. It is a turn average, not an instantaneous
        // rate — the live "≈ n/s" while streaming is the estimate.
        if self.completion_tokens > 0 && self.llm_ms > 0 {
            let per_s = self.completion_tokens as f64 / (self.llm_ms as f64 / 1000.0);
            s.push_str(&format!(" · {:.0} tok/s", per_s));
        }
        if let Some(pct) = self.cache_pct() {
            s.push_str(&format!(" · cache {pct}%"));
        }
        // Failures and brain hits only when they exist: a segment that is almost
        // always "0" costs width on every line.
        if self.failures > 0 {
            s.push_str(&format!(" · {} failed", self.failures));
        }
        if self.brain_hits > 0 {
            s.push_str(&format!(" · brain {}", self.brain_hits));
        }
        s
    }
}

/// data_dir of the running instance (for /metrics). Set at startup.
static DATA_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// This workspace's todo file. Set at startup, because the list is per project
/// and the TUI helpers only receive `App`.
static TODO_FILE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The TUI event channel, published at startup. It lets a function that is reached
/// from several places — like the clipboard paste — hand its result back to the
/// event loop without a sender being threaded through every signature.
static EVENT_TX: std::sync::OnceLock<mpsc::UnboundedSender<TuiEvent>> = std::sync::OnceLock::new();

/// Read the pending attachment paths into sendable form.
///
/// Failures are collected, never fatal: one unreadable file must not lose the
/// whole message. The reading is done here (in the turn task), because base64 of
/// an image is not free and must not block the key thread.
fn build_attachments(paths: &[String]) -> (Vec<rijo_core::llm::Attachment>, Vec<String>) {
    let mut out = Vec::with_capacity(paths.len());
    let mut notes = Vec::new();
    for path in paths {
        match rijo_core::llm::load_attachment(path) {
            Ok(a) => out.push(a),
            Err(e) => notes.push(e),
        }
    }
    (out, notes)
}

/// One row of the todo strip.
#[derive(Clone, Debug, PartialEq)]
struct TodoView {
    content: String,
    status: String,
}

/// Read the list the `todo_list` tool persists. Best-effort: a missing or
/// unreadable file means "no todos", never an error (the strip is decoration, the
/// tool owns the data).
fn load_todos() -> Vec<TodoView> {
    let Some(path) = TODO_FILE.get() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(&text)
    else {
        return Vec::new();
    };
    items
        .iter()
        .map(|t| TodoView {
            content: t["content"].as_str().unwrap_or("").to_string(),
            status: t["status"].as_str().unwrap_or("pending").to_string(),
        })
        .collect()
}

fn build_operations_items(
    processes: &[rijo_core::process::Info],
    todos: &[TodoView],
    cron_jobs: &[(String, String)],
) -> Vec<OperationsItem> {
    let mut items = Vec::new();
    for process in processes {
        let status = if process.running {
            format!("Running · {}s", process.uptime_ms / 1000)
        } else {
            format!("Exited · code {}", process.exit_code.unwrap_or(-1))
        };
        items.push(OperationsItem {
            title: format!("{} · {status}", process.id),
            detail: format!("PID {} · background process", process.pid),
            prompt: if process.running {
                format!(
                    "Poll background process {} and summarize new output",
                    process.id
                )
            } else {
                format!("Show the final status of background process {}", process.id)
            },
        });
    }
    for todo in todos {
        let (glyph, _) = todo_glyph(&todo.status);
        items.push(OperationsItem {
            title: format!("{glyph} {}", todo.content),
            detail: format!("Todo · {}", todo.status.replace('_', " ")),
            prompt: format!("Continue the task: {}", todo.content),
        });
    }
    for (id, interval) in cron_jobs {
        items.push(OperationsItem {
            title: format!("Scheduled job · {id}"),
            detail: format!("Every {interval} · prompt hidden"),
            prompt: format!("Inspect scheduled job {id}"),
        });
    }
    items
}

fn load_cron_summaries() -> Vec<(String, String)> {
    let Some(data_dir) = DATA_DIR.get() else {
        return Vec::new();
    };
    let dir = data_dir.join("cron").join("jobs");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut jobs = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|ty| ty.is_file()).unwrap_or(false) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > 64 * 1024 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(job) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let id = job
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                entry
                    .path()
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            });
        let interval = job
            .get("interval_secs")
            .and_then(|v| v.as_u64())
            .map(|seconds| format!("{seconds}s"))
            .unwrap_or_else(|| "unknown interval".to_string());
        if let Some(id) = id {
            jobs.push((id, interval));
        }
    }
    jobs.sort_by(|a, b| a.0.cmp(&b.0));
    jobs
}

/// Glyph and style for a todo status. Narrow glyphs only: a wide emoji would
/// shift the column.
fn todo_glyph(status: &str) -> (&'static str, Style) {
    match status {
        "in_progress" => (
            "▸",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        "completed" => ("✔", Style::default().fg(theme::OK)),
        "cancelled" => (
            "✗",
            Style::default()
                .fg(theme::DIM)
                .add_modifier(Modifier::CROSSED_OUT),
        ),
        _ => ("☐", theme::dim()),
    }
}

/// The running animation: ten braille frames, one cell wide and smooth in every
/// monospace font. The old two-cell «◯◒» frames shifted the line by one column
/// each tick and rendered inconsistently.
fn spinner_glyph(frame: usize) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[frame % FRAMES.len()]
}

/// The "Rijo Code is working HERE" marker of the todo strip: a live spinner while
/// a turn runs (so the strip visibly lives), a static arrow when nothing runs.
fn working_glyph(app: &App) -> &'static str {
    if app.running {
        spinner_glyph(app.spinner_frame)
    } else {
        "▸"
    }
}

/// Re-read `todos.json` when it changed (mtime + size guard). The tick calls this
/// so the strip stays live even if a change event was missed — and 12 stats/s on a
/// small file cost nothing.
fn refresh_todos(app: &mut App) -> bool {
    let Some(path) = TODO_FILE.get() else {
        return false;
    };
    let Ok(md) = std::fs::metadata(path) else {
        if !app.todos.is_empty() {
            app.todos.clear();
            app.todos_sig = None;
            return true;
        }
        return false;
    };
    let sig = (
        md.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
        md.len(),
    );
    if app.todos_sig == Some(sig) {
        return false;
    }
    app.todos_sig = Some(sig);
    let fresh = load_todos();
    // Only a REAL change counts as maintenance: a write that keeps the same statuses
    // must not silence the stale marker (models do exactly that — real report).
    if fresh != app.todos {
        app.todos = fresh;
        app.calls_since_todos = 0;
        return true;
    }
    false
}

/// State of an open question overlay. The turn that asked is blocked on `tx`.
struct AskState {
    questions: Vec<AskQuestion>,
    current: usize,
    /// Chosen row per question (an index into `rows(i)`).
    choice: Vec<usize>,
    /// Typed free text per question.
    text: Vec<String>,
    tx: Option<Arc<tokio::sync::oneshot::Sender<Vec<(String, String)>>>>,
}

impl AskState {
    fn new(
        questions: Vec<AskQuestion>,
        tx: Arc<tokio::sync::oneshot::Sender<Vec<(String, String)>>>,
    ) -> Self {
        let n = questions.len();
        Self {
            questions,
            current: 0,
            choice: vec![0; n],
            text: vec![String::new(); n],
            tx: Some(tx),
        }
    }

    /// The rows shown for question `i`: its options, plus a free-text row when
    /// allowed (or when there are no options at all).
    fn rows(&self, i: usize) -> Vec<String> {
        let q = &self.questions[i];
        let mut rows: Vec<String> = q.options.iter().map(|o| o.label.clone()).collect();
        if q.allow_free_text || rows.is_empty() {
            rows.push("Other …".to_string());
        }
        rows
    }

    /// The index of the free-text row, or `usize::MAX` when there is none.
    fn free_text_row(&self, i: usize) -> usize {
        let q = &self.questions[i];
        if q.allow_free_text || q.options.is_empty() {
            q.options.len()
        } else {
            usize::MAX
        }
    }

    fn is_free_text(&self, i: usize) -> bool {
        self.choice[i] == self.free_text_row(i)
    }

    /// The answer for question `i`.
    fn answer(&self, i: usize) -> String {
        if self.is_free_text(i) {
            return self.text[i].trim().to_string();
        }
        self.questions[i]
            .options
            .get(self.choice[i])
            .map(|o| o.label.clone())
            .unwrap_or_default()
    }

    fn answers(&self) -> Vec<(String, String)> {
        (0..self.questions.len())
            .map(|i| (self.questions[i].id.clone(), self.answer(i)))
            .collect()
    }
}

/// Keys while the question overlay is open. Enter advances (and on the last
/// question answers); Esc answers with whatever is selected so far. Everything
/// else is option navigation or free text.
fn handle_ask_key(app: &mut App, key: crossterm::event::KeyEvent) {
    let Some(mut ask) = app.ask.take() else {
        return;
    };
    let i = ask.current;
    let rows = ask.rows(i).len().max(1);
    let in_text = ask.is_free_text(i);
    let mut done = false;
    match key.code {
        KeyCode::Up => ask.choice[i] = ask.choice[i].saturating_sub(1),
        KeyCode::Down => ask.choice[i] = (ask.choice[i] + 1).min(rows - 1),
        KeyCode::Char(c) if ('1'..='9').contains(&c) => {
            let idx = (c as usize) - ('1' as usize);
            if idx < rows {
                ask.choice[i] = idx;
            }
        }
        KeyCode::Char(c) if in_text => ask.text[i].push(c),
        KeyCode::Backspace if in_text => {
            ask.text[i].pop();
        }
        KeyCode::Tab => ask.current = (ask.current + 1) % ask.questions.len(),
        KeyCode::BackTab => {
            ask.current = (ask.current + ask.questions.len() - 1) % ask.questions.len()
        }
        KeyCode::Enter => {
            if in_text && ask.text[i].trim().is_empty() {
                // Nothing typed yet — stay until there is an answer.
            } else if ask.current + 1 < ask.questions.len() {
                ask.current += 1;
            } else {
                done = true;
            }
        }
        KeyCode::Esc => done = true,
        _ => {}
    }
    if done {
        let answers = ask.answers();
        let summary = answers
            .iter()
            .map(|(_, a)| {
                if a.is_empty() {
                    "(skipped)".to_string()
                } else {
                    a.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" · ");
        if let Some(tx) = ask.tx.take() {
            if let Ok(sender) = Arc::try_unwrap(tx) {
                let _ = sender.send(answers);
            }
        }
        app.push(EntryRole::Status, format!("answered: {summary}"));
    } else {
        app.ask = Some(ask);
    }
}

/// The question overlay: a centred, accented frame. Everything that wants an
/// answer gets a frame — the absence of a frame is the signal for "this is just
/// transcript".
fn render_ask(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some(ask) = &app.ask else {
        return;
    };
    let width = area.width.saturating_sub(8).clamp(30, 76);
    let body: usize = (0..ask.questions.len())
        .map(|i| 1 + ask.rows(i).len())
        .sum();
    let height = (body as u16 + 2).min(area.height.saturating_sub(2)).max(3);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::rounded())
        .border_style(Style::default().fg(theme::ACCENT))
        .title(Span::styled(
            format!(" ? question {}/{} ", ask.current + 1, ask.questions.len()),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
    let mut lines: Vec<Line> = Vec::new();
    for (i, q) in ask.questions.iter().enumerate() {
        let head = if i == ask.current {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            theme::dim()
        };
        lines.push(Line::from(Span::styled(
            format!("{}. {}", i + 1, q.question),
            head,
        )));
        let free = ask.free_text_row(i);
        for (j, label) in ask.rows(i).iter().enumerate() {
            let selected = i == ask.current && ask.choice[i] == j;
            let marker = if selected { "  ▸ " } else { "    " };
            let mut style = if j == free {
                theme::dim()
            } else {
                Style::default().fg(theme::DIM)
            };
            if q.options.get(j).map(|o| o.recommended).unwrap_or(false) {
                style = Style::default().fg(theme::OK);
            }
            if selected {
                style = style.add_modifier(Modifier::BOLD);
            }
            let text = if j == free && selected && !ask.text[i].is_empty() {
                format!("{label} [{}]", ask.text[i])
            } else {
                label.clone()
            };
            lines.push(Line::from(vec![
                Span::styled(marker.to_string(), Style::default().fg(theme::ACCENT)),
                Span::styled(text, style),
            ]));
        }
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(ratatui::widgets::Wrap { trim: true }),
        rect,
    );
}

impl App {
    fn new() -> Self {
        Self {
            entries: vec![],
            render_cache: Vec::new(),
            selection_signature: None,
            stream_buf: None,
            stream_finalized: false,
            input: String::new(),
            cursor: 0,
            conversation_history: vec![],
            autoscroll: None,
            phase: String::new(),
            turn_started: None,
            thinking_effort: "med".to_string(),
            tool_started: None,
            verbose: false,
            reasoning_buf: String::new(),
            reasoning_idx: None,
            open_tools: std::collections::HashMap::new(),
            input_history: vec![],
            history_idx: None,
            bottom_anchor: None,
            spinner_frame: 0,
            todos_sig: None,
            calls_since_todos: 0,
            last_chat_area: None,
            last_chat_scroll: 0,
            last_row_activity: None,
            last_row_todo: None,
            toast: None,
            picker: None,
            palette: None,
            tool_catalog: Vec::new(),
            tool_explorer: None,
            skills_catalog: Vec::new(),
            skills_view: None,
            runtime_lines: Vec::new(),
            runtime_scroll: None,
            operations_items: Vec::new(),
            operations_selected: None,
            history_view: None,
            state: None,
            memory_items: Vec::new(),
            memory_selected: None,
            archived_activity: Vec::new(),
            compacted_activity_count: 0,
            activity_view: None,
            confirm: None,
            select_panel: tui_panel_select::SelectablePanel::new(),
            selected_prompt: -1,
            running: false,
            last_stats: None,
            context_window: 0,
            live_ctx_chars: 0,
            live_ctx_messages: 0,
            session_turns: 0,
            todos: Vec::new(),
            todos_expanded: false,
            ask: None,
            attachments: Vec::new(),
            session_prompt_tokens: 0,
            session_completion_tokens: 0,
        }
    }

    fn push(&mut self, role: EntryRole, text: impl Into<String>) {
        self.entries.push(ChatEntry {
            role,
            text: text.into(),
            detail: None,
            expanded: false,
        });
    }
}

// ─── Global cancel flag (Ctrl+C while running) ───────────────────────────

// (the old process-wide `CANCEL` static was removed — cancellation goes through
// `app.cancel` / `AppState::cancel`)
/// Cached layout metrics published by draw() for key-handler clamping.
/// Updated every draw() cycle so scroll maths stays in sync with reality.
static CACHED_TOTAL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static CACHED_VISIBLE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Upper bound for bottom_anchor (max_scroll), refreshed each draw.
static CACHED_MAX_SCROLL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// ─── Main loop ───────────────────────────────────────────────────────────

pub async fn run(
    executor: Arc<rijo_core::executor::Executor>,
    model_name: &str,
    tool_count: usize,
    resume_history: Vec<(String, String)>,
) {
    let res = run_inner(executor, model_name, tool_count, resume_history).await;
    let _ = execute!(stdout(), crossterm::event::DisableMouseCapture);
    let _ = execute!(stdout(), crossterm::event::DisableBracketedPaste);
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), LeaveAlternateScreen);
    if let Err(e) = res {
        eprintln!("TUI error: {e}");
    }
}

async fn run_inner(
    executor: Arc<rijo_core::executor::Executor>,
    model_name: &str,
    tool_count: usize,
    resume_history: Vec<(String, String)>,
) -> Result<(), String> {
    // remember the data_dir so /metrics reads this instance's metrics file
    let _ = DATA_DIR.set(executor.data_dir());
    let state_for_todos = executor.state();
    let _ = TODO_FILE.set(rijo_core::tools::todo_tool::path_for(
        &state_for_todos.config.data_dir,
        &state_for_todos.config.workspace,
    ));
    let context_window = executor.context_window();
    enable_raw_mode().map_err(|e| e.to_string())?;
    // Windows: the selection crate would pipe copied text into `clip.exe`, which
    // decodes the console code page rather than UTF-8 — the chat's `·`, `→`, `✓`
    // came back as mojibake. Turn its clipboard off; `copy_to_clipboard` (which
    // asks PowerShell to read UTF-8) does the copy instead. On Unix the crate's
    // xclip/wl-copy preference is kept, because some terminals ignore OSC 52.
    #[cfg(windows)]
    tui_panel_select::set_clipboard_mode(tui_panel_select::ClipboardMode::None);
    execute!(
        stdout(),
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste
    )
    .map_err(|e| e.to_string())?;
    // Mouse tracking: full control (1000+1002+1006).
    // Own text selection via mouse (double-click word, drag range).
    // Copy: /copy or Enter after a text selection.

    let backend = ratatui::backend::CrosstermBackend::new(stdout());
    let mut terminal = ratatui::Terminal::new(backend).map_err(|e| e.to_string())?;
    terminal.clear().map_err(|e| e.to_string())?;

    let mut app = App::new();
    app.context_window = context_window;
    let state = executor.state();
    app.state = Some(state.clone());
    app.tool_catalog = build_tool_catalog(&state.tools);
    app.skills_catalog = rijo_core::tools::skills_tool::list_skills(&state);
    app.thinking_effort = state.config.llm.thinking_effort.clone();
    let router_loaded = state
        .router
        .lock()
        .map(|router| router.is_some())
        .unwrap_or(false);
    app.runtime_lines = build_runtime_lines(
        &state.config,
        app.tool_catalog.len(),
        app.tool_catalog.iter().any(|tool| tool.toolset == "box"),
        router_loaded,
    );
    // Show an existing todo list right away (the file survives restarts).
    app.todos = load_todos();
    app.push(
        EntryRole::Status,
        format!(
            "Rijo Code · {model_name} · {tool_count} tools — Enter=send · /tools · /runtime · /operations · /copy · ↑/↓ history or scroll · PgUp/PgDn=scroll · Alt/Shift+Enter=newline · Ctrl+C=quit/cancel"
        ),
    );
    // resume: load previous session messages into the chat view AND the
    // conversation_history so the LLM sees prior context immediately
    for (role, content) in resume_history {
        let r = match role.as_str() {
            "user" => EntryRole::User,
            "assistant" => EntryRole::Assistant,
            _ => EntryRole::Status,
        };
        if role == "user" || role == "assistant" {
            app.conversation_history.push(rijo_core::llm::ChatMessage {
                role: role.clone(),
                content: Some(content.clone()),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                parts: None,
            });
        }
        app.push(r, content);
    }
    if app.entries.len() > 1 {
        app.push(EntryRole::Status, "— session resumed —".to_string());
    }

    // Windows Terminal binds Ctrl+V to its own paste action by default. With an
    // image on the clipboard there is no text to paste, so the keystroke never
    // reaches the app at all — upstream is still open (microsoft/terminal#19517).
    // Phrased as a REQUIREMENT, not as a claim about the current state: the user
    // may well have unbound it already (docs/features/tui-attachments.md).
    if std::env::var_os("WT_SESSION").is_some() {
        app.push(
            EntryRole::Status,
            "Ctrl+V needs Windows Terminal's paste binding unbound — /paste always works."
                .to_string(),
        );
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<TuiEvent>();
    let _ = EVENT_TX.set(tx.clone());

    // Core events: the engine publishes `UiEvent` on `AppState::ui_tx`. The TUI
    // owns that sender, and a forwarding task wraps every event into
    // `TuiEvent::Core` so the event loop only ever reads one channel.
    let (core_tx, mut core_rx) = mpsc::unbounded_channel::<UiEvent>();
    {
        let state = executor.state();
        *state.ui_tx.lock().unwrap() = Some(core_tx.clone());
    }
    {
        let tx_core = tx.clone();
        tokio::spawn(async move {
            while let Some(event) = core_rx.recv().await {
                if tx_core.send(TuiEvent::Core(event)).is_err() {
                    break;
                }
            }
        });
    }

    // fs watcher: config.yaml + skills/** + staging → core UiEvents
    {
        let tx_w = core_tx.clone();
        let _ = std::thread::Builder::new()
            .name("watcher".into())
            .spawn(move || {
                if let Err(e) = rijo_core::watcher::spawn(tx_w) {
                    tracing::warn!("[hot-reload] watcher failed: {e}");
                }
            });
    }

    // key thread: blocking crossterm reader → channel.
    //
    // crossterm 0.29 implements `Event::Paste` only on Unix; on Windows a paste
    // arrives as a burst of key events, and every line break in it reached the
    // handler as Enter — which SENT the prompt in the middle of the paste. The fix
    // is the Enter test below: a line break inside a paste becomes a newline
    // before it can reach the send path. Every character still travels as its own
    // key event (so `@`/`/` keep opening, and typing adds no latency); the
    // per-character redraw cost is handled by skipping redraws in the loop.
    let tx_keys = tx.clone();
    std::thread::spawn(move || {
        let mut last: Option<std::time::Instant> = None;
        // An event read during the Enter lookahead is handed to the next pass here
        // instead of being swallowed.
        let mut pending: Option<CEvent> = None;
        loop {
            let ev = match pending.take() {
                Some(ev) => ev,
                None => match event::read() {
                    Ok(e) => e,
                    Err(_) => break,
                },
            };
            let first = match ev {
                // A Release event accompanies every press on Windows; forwarding it
                // made each typed character appear twice.
                CEvent::Key(k) => {
                    if k.kind == crossterm::event::KeyEventKind::Release {
                        continue;
                    }
                    k
                }
                CEvent::Mouse(m) => {
                    if tx_keys.send(TuiEvent::Mouse(m)).is_err() {
                        break;
                    }
                    continue;
                }
                CEvent::Paste(text) => {
                    if tx_keys.send(TuiEvent::Paste(text)).is_err() {
                        break;
                    }
                    continue;
                }
                _ => continue,
            };

            let now = std::time::Instant::now();
            let burst = is_paste_burst(last.map(|t| now.duration_since(t)));
            last = Some(now);

            // Only an unmodified Enter is ambiguous: it SENDS, unless it is a line
            // break inside a paste. Two signals decide that, and a deliberate Enter
            // produces neither:
            //   * a character typed right before it (BURST_GAP — a human cannot
            //     press two keys that fast), or
            //   * more KEY input already waiting behind it.
            // A pending mouse event is deliberately NOT a signal: it must never turn
            // a send into a newline.
            let out = if first.code == KeyCode::Enter && first.modifiers.is_empty() {
                let mut more_pending = false;
                if !burst && matches!(event::poll(PASTE_LOOKAHEAD), Ok(true)) {
                    if let Ok(next) = event::read() {
                        more_pending = matches!(next, CEvent::Key(_));
                        // Hand it to the next pass either way — never dropped.
                        pending = Some(next);
                    }
                }
                if enter_is_pasted_newline(burst, more_pending) {
                    TuiEvent::Paste("\n".to_string())
                } else {
                    TuiEvent::Key(first)
                }
            } else {
                TuiEvent::Key(first)
            };
            if tx_keys.send(out).is_err() {
                break;
            }
        }
    });

    let mut quit = false;
    let mut frame_dirty = true;
    while !quit {
        if QUIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            quit = true;
            continue;
        }
        // Skip the redraw while more input is queued: a paste arrives as hundreds
        // of key events, and repainting the whole frame for each one is what made
        // it "crawl in". The final state is drawn once the queue drains.
        if frame_dirty && rx.is_empty() {
            draw(&mut terminal, &mut app)?;
            frame_dirty = false;
        }

        let ev = tokio::select! {
            ev = rx.recv() => match ev { Some(e) => e, None => break },
            _ = tokio::time::sleep(tick_interval(&app)) => {
                if app.running {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                }
                // Keep the todo strip live even without a change event.
                let todos_changed = refresh_todos(&mut app);
                let mut toast_expired = false;
                if let Some((_, n)) = app.toast.as_mut() {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        app.toast = None;
                        toast_expired = true;
                    }
                }
                // Continuous edge auto-scroll: 1 row per tick while armed
                if let Some(up) = app.autoscroll {
                    if up { scroll_up(&mut app, 1); } else { scroll_down(&mut app, 1); }
                }
                if tick_requires_redraw(&app, todos_changed, toast_expired) {
                    frame_dirty = true;
                }
                continue;
            }
        };

        frame_dirty = true;
        match ev {
            TuiEvent::Key(key) => {
                // Ctrl+C is global, even while a search/picker/palette owns input.
                if let (KeyCode::Char('c'), true) =
                    (key.code, key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    match ctrl_c_action(app.running) {
                        CtrlCAction::Quit => quit = true,
                        CtrlCAction::Cancel => {
                            executor
                                .state()
                                .cancel
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                            app.toast = Some((" cancel requested… ".to_string(), 25));
                        }
                    }
                    continue;
                }
                // Confirm dialog intercepts all keys while open
                if app.confirm.is_some() {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                            if let Some((tx, _)) = app.confirm.take() {
                                // send() takes self, so try_unwrap the Arc
                                if let Ok(sender) = Arc::try_unwrap(tx) {
                                    let _ = sender.send(true);
                                }
                            }
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            if let Some((tx, _)) = app.confirm.take() {
                                if let Ok(sender) = Arc::try_unwrap(tx) {
                                    let _ = sender.send(false);
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // question overlay intercepts all keys while open
                if app.ask.is_some() {
                    handle_ask_key(&mut app, key);
                    continue;
                }

                // Tool directory is a local metadata view; it never dispatches a
                // tool or makes a provider request.
                if app.tool_explorer.is_some() {
                    match key.code {
                        KeyCode::Esc => app.tool_explorer = None,
                        KeyCode::Up => {
                            if let Some((query, selected)) = app.tool_explorer.as_mut() {
                                *selected = selected.saturating_sub(1);
                                let len = filtered_tool_indices(&app.tool_catalog, query).len();
                                *selected = (*selected).min(len.saturating_sub(1));
                            }
                        }
                        KeyCode::Down => {
                            if let Some((query, selected)) = app.tool_explorer.as_mut() {
                                let len = filtered_tool_indices(&app.tool_catalog, query).len();
                                *selected = selected.saturating_add(1).min(len.saturating_sub(1));
                            }
                        }
                        KeyCode::Enter => {
                            let selected_entry =
                                app.tool_explorer.as_ref().and_then(|(query, selected)| {
                                    filtered_tool_indices(&app.tool_catalog, query)
                                        .get(*selected)
                                        .and_then(|i| app.tool_catalog.get(*i))
                                        .map(|entry| entry.name.clone())
                                });
                            if let Some(name) = selected_entry {
                                let prefix = format!("Please use {name} to ");
                                app.input.insert_str(0, &prefix);
                                app.cursor = prefix.len();
                                app.history_idx = None;
                                app.tool_explorer = None;
                            }
                        }
                        KeyCode::Backspace => {
                            if let Some((query, selected)) = app.tool_explorer.as_mut() {
                                query.pop();
                                *selected = 0;
                            }
                        }
                        KeyCode::Char(c) if allows_text_entry(key.modifiers) => {
                            if let Some((query, selected)) = app.tool_explorer.as_mut() {
                                query.push(c);
                                *selected = 0;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if app.skills_view.is_some() {
                    match key.code {
                        KeyCode::Esc => app.skills_view = None,
                        KeyCode::Up => {
                            if let Some((query, selected)) = app.skills_view.as_mut() {
                                *selected = selected.saturating_sub(1);
                                let len = filtered_skill_indices(&app.skills_catalog, query).len();
                                *selected = (*selected).min(len.saturating_sub(1));
                            }
                        }
                        KeyCode::Down => {
                            if let Some((query, selected)) = app.skills_view.as_mut() {
                                let len = filtered_skill_indices(&app.skills_catalog, query).len();
                                *selected = selected.saturating_add(1).min(len.saturating_sub(1));
                            }
                        }
                        KeyCode::Enter => {
                            let selected_skill =
                                app.skills_view.as_ref().and_then(|(query, selected)| {
                                    filtered_skill_indices(&app.skills_catalog, query)
                                        .get(*selected)
                                        .and_then(|i| app.skills_catalog.get(*i))
                                        .map(|skill| skill.name.clone())
                                });
                            if let Some(name) = selected_skill {
                                insert_prompt_template(
                                    &mut app,
                                    &format!("Load skill '{name}' and apply its guidance to: "),
                                );
                                app.skills_view = None;
                            }
                        }
                        KeyCode::Backspace => {
                            if let Some((query, selected)) = app.skills_view.as_mut() {
                                query.pop();
                                *selected = 0;
                            }
                        }
                        KeyCode::Char(c) if allows_text_entry(key.modifiers) => {
                            if let Some((query, selected)) = app.skills_view.as_mut() {
                                query.push(c);
                                *selected = 0;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if app.runtime_scroll.is_some() {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter => app.runtime_scroll = None,
                        KeyCode::Up => {
                            if let Some(scroll) = app.runtime_scroll.as_mut() {
                                *scroll = scroll.saturating_sub(1);
                            }
                        }
                        KeyCode::Down => {
                            if let Some(scroll) = app.runtime_scroll.as_mut() {
                                *scroll = scroll
                                    .saturating_add(1)
                                    .min(app.runtime_lines.len().saturating_sub(1) as u16);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if app.operations_selected.is_some() {
                    match key.code {
                        KeyCode::Esc => app.operations_selected = None,
                        KeyCode::Up => {
                            if let Some(selected) = app.operations_selected.as_mut() {
                                *selected = selected.saturating_sub(1);
                            }
                        }
                        KeyCode::Down => {
                            if let Some(selected) = app.operations_selected.as_mut() {
                                *selected = selected
                                    .saturating_add(1)
                                    .min(app.operations_items.len().saturating_sub(1));
                            }
                        }
                        KeyCode::Enter => {
                            let prompt = app
                                .operations_selected
                                .and_then(|selected| app.operations_items.get(selected))
                                .map(|item| item.prompt.clone());
                            if let Some(prompt) = prompt {
                                insert_operation_prompt(&mut app, &prompt);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if app.history_view.is_some() {
                    match key.code {
                        KeyCode::Esc => app.history_view = None,
                        KeyCode::Up => {
                            if let Some(view) = app.history_view.as_mut() {
                                view.selected = view.selected.saturating_sub(1);
                            }
                        }
                        KeyCode::Down => {
                            if let Some(view) = app.history_view.as_mut() {
                                view.selected = view
                                    .selected
                                    .saturating_add(1)
                                    .min(view.results.len().saturating_sub(1));
                            }
                        }
                        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            let session_id = app.history_view.as_ref().and_then(|view| {
                                (view.kind == SearchMode::History)
                                    .then(|| view.results.get(view.selected))
                                    .flatten()
                                    .map(|hit| hit.session_id.clone())
                            });
                            if let Some(session_id) = session_id {
                                request_session_resume(
                                    &mut app,
                                    executor.clone(),
                                    tx.clone(),
                                    session_id,
                                );
                            }
                        }
                        KeyCode::Enter => {
                            let selected_hit = app.history_view.as_ref().and_then(|view| {
                                view.results
                                    .get(view.selected)
                                    .cloned()
                                    .map(|hit| (view.kind, hit))
                            });
                            if let Some((kind, hit)) = selected_hit {
                                insert_prompt_template(&mut app, &search_hit_prompt(kind, &hit));
                                app.history_view = None;
                            } else if !app
                                .history_view
                                .as_ref()
                                .map(|view| view.searching)
                                .unwrap_or(false)
                            {
                                start_history_search(&mut app, executor.state(), tx.clone());
                            }
                        }
                        KeyCode::Backspace => {
                            if let Some(view) = app.history_view.as_mut() {
                                view.query.pop();
                                view.results.clear();
                                view.selected = 0;
                                view.searching = false;
                                view.error = None;
                            }
                        }
                        KeyCode::Char(c) if allows_text_entry(key.modifiers) => {
                            if let Some(view) = app.history_view.as_mut() {
                                view.query.push(c);
                                view.results.clear();
                                view.selected = 0;
                                view.searching = false;
                                view.error = None;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if app.memory_selected.is_some() {
                    match key.code {
                        KeyCode::Esc => app.memory_selected = None,
                        KeyCode::Up => {
                            if let Some(selected) = app.memory_selected.as_mut() {
                                *selected = selected.saturating_sub(1);
                            }
                        }
                        KeyCode::Down => {
                            if let Some(selected) = app.memory_selected.as_mut() {
                                *selected = selected
                                    .saturating_add(1)
                                    .min(app.memory_items.len().saturating_sub(1));
                            }
                        }
                        KeyCode::Enter => {
                            let item = app
                                .memory_selected
                                .and_then(|selected| app.memory_items.get(selected).cloned());
                            if let Some(item) = item {
                                insert_prompt_template(
                                    &mut app,
                                    &format!(
                                        "Use this saved {} memory entry as context: {}",
                                        item.target, item.content
                                    ),
                                );
                                app.memory_selected = None;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if app.activity_view.is_some() {
                    match key.code {
                        KeyCode::Esc => app.activity_view = None,
                        KeyCode::Up => {
                            if let Some((query, selected)) = app.activity_view.as_mut() {
                                *selected = selected.saturating_sub(1);
                                let len =
                                    filtered_activity_indices(&app.archived_activity, query).len();
                                *selected = (*selected).min(len.saturating_sub(1));
                            }
                        }
                        KeyCode::Down => {
                            if let Some((query, selected)) = app.activity_view.as_mut() {
                                let len =
                                    filtered_activity_indices(&app.archived_activity, query).len();
                                *selected = selected.saturating_add(1).min(len.saturating_sub(1));
                            }
                        }
                        KeyCode::Enter => {
                            let item = app.activity_view.as_ref().and_then(|(query, selected)| {
                                filtered_activity_indices(&app.archived_activity, query)
                                    .get(*selected)
                                    .and_then(|i| app.archived_activity.get(*i))
                                    .cloned()
                            });
                            if let Some(item) = item {
                                insert_prompt_template(
                                    &mut app,
                                    &format!("Review this archived activity summary: {item}"),
                                );
                                app.activity_view = None;
                            }
                        }
                        KeyCode::Backspace => {
                            if let Some((query, selected)) = app.activity_view.as_mut() {
                                query.pop();
                                *selected = 0;
                            }
                        }
                        KeyCode::Char(c) if allows_text_entry(key.modifiers) => {
                            if let Some((query, selected)) = app.activity_view.as_mut() {
                                query.push(c);
                                *selected = 0;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // command palette intercepts keys while open
                if app.palette.is_some() {
                    match key.code {
                        KeyCode::Up => {
                            if let Some((_, sel, _)) = app.palette.as_mut() {
                                *sel = sel.saturating_sub(1);
                            }
                        }
                        KeyCode::Down => {
                            if let Some((_, sel, items)) = app.palette.as_mut() {
                                *sel = (*sel + 1).min(items.len().saturating_sub(1));
                            }
                        }
                        KeyCode::Enter => {
                            let q = app
                                .palette
                                .as_ref()
                                .map(|(q, _, _)| q.clone())
                                .unwrap_or_default();
                            let results = fuzzy_commands(&q, 10);
                            if let Some(cmd) = results
                                .get(app.palette.as_ref().map(|(_, s, _)| *s).unwrap_or(0))
                                .copied()
                            {
                                app.palette = None;
                                // /exit must quit: handle by pushing a marker entry the caller cannot see...
                                // We handle quit by checking after execute.
                                if palette_command_needs_argument(cmd) {
                                    insert_palette_command_prefix(&mut app, cmd);
                                } else {
                                    match cmd {
                                        "/exit" | "/quit" | "/q" => {
                                            quit = true;
                                        }
                                        other => execute_command(&mut app, other),
                                    }
                                }
                            }
                        }
                        KeyCode::Char(c) if allows_text_entry(key.modifiers) => {
                            if let Some((q, sel, items)) = app.palette.as_mut() {
                                q.push(c);
                                *items = fuzzy_commands(q, 30);
                                *sel = 0;
                            }
                        }
                        KeyCode::Backspace => {
                            if let Some((q, sel, items)) = app.palette.as_mut() {
                                q.pop();
                                *items = fuzzy_commands(q, 30);
                                *sel = 0;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // @file picker intercepts keys while open
                if app.picker.is_some() {
                    match key.code {
                        KeyCode::Up => {
                            if let Some((_, sel, _files)) = app.picker.as_mut() {
                                *sel = sel.saturating_sub(1);
                            }
                        }
                        KeyCode::Down => {
                            if let Some((_, sel, files)) = app.picker.as_mut() {
                                *sel = (*sel + 1).min(files.len().saturating_sub(1));
                            }
                        }
                        KeyCode::Enter => {
                            if let Some((q, sel, _)) = app.picker.clone() {
                                let results = fuzzy_files(&q, 30);
                                if let Some(file) = results.get(sel).cloned() {
                                    app.input = format!("@{} ", file);
                                    app.cursor = app.input.len();
                                    app.picker = None;
                                } else {
                                    app.picker = None;
                                }
                            }
                        }
                        KeyCode::Esc => {
                            app.picker = None;
                        }
                        KeyCode::Char(c) if allows_text_entry(key.modifiers) => {
                            if let Some((q, sel, files)) = app.picker.as_mut() {
                                q.push(c);
                                *files = fuzzy_files(q, 30);
                                *sel = 0;
                            }
                        }
                        KeyCode::Backspace => {
                            if let Some((q, sel, files)) = app.picker.as_mut() {
                                q.pop();
                                *files = fuzzy_files(q, 30);
                                *sel = 0;
                                if q.is_empty() {
                                    app.picker = None;
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Esc
                        if app.running
                            && app.picker.is_none()
                            && app.palette.is_none()
                            && app.confirm.is_none() =>
                    {
                        // Esc while streaming: cooperative cancel of turn + tools
                        executor
                            .state()
                            .cancel
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        app.toast = Some((" cancel requested… ".to_string(), 25));
                    }
                    // Enter sends the prompt; with a modifier it inserts a line
                    // break so a multi-line prompt can be written, not only pasted.
                    KeyCode::Enter if !key.modifiers.is_empty() => {
                        let at = app.cursor.min(app.input.len());
                        app.input.insert(at, '\n');
                        app.cursor = at + 1;
                        app.history_idx = None;
                    }
                    KeyCode::Enter => {
                        let text = app.input.trim().to_string();
                        app.input.clear();
                        app.cursor = 0;
                        if text.is_empty() {
                            continue;
                        }
                        if app.running {
                            app.push(
                                EntryRole::Status,
                                "Agent is running — please wait.".to_string(),
                            );
                            continue;
                        }
                        // Slash-Commands
                        if text.starts_with('/') {
                            let command = text.split_whitespace().next().unwrap_or("");
                            let argument = text
                                .split_once(char::is_whitespace)
                                .map(|(_, rest)| rest.trim())
                                .unwrap_or("");
                            match canonical_command(command) {
                                Some("/thinking") => {
                                    set_thinking_effort(&mut app, &argument.to_lowercase());
                                }
                                Some("/attach") => {
                                    // Everything after the command is the path, so a
                                    // filename with spaces works.
                                    if argument.is_empty() {
                                        app.push(
                                            EntryRole::Status,
                                            "Usage: /attach <path>  (image or text file)"
                                                .to_string(),
                                        );
                                    } else if !std::path::Path::new(argument).exists() {
                                        app.push(
                                            EntryRole::Status,
                                            format!("not found: {argument}"),
                                        );
                                    } else {
                                        app.attachments.push(argument.to_string());
                                        let mut note = format!("attached: {argument}");
                                        if rijo_core::llm::is_image_path(argument)
                                            && !rijo_core::llm::model_supports_vision(
                                                &executor.state().config.llm.model,
                                            )
                                        {
                                            note.push_str(
                                                "  (model takes no images — RIJO_VISION=on forces it)",
                                            );
                                        }
                                        app.push(EntryRole::Status, note);
                                    }
                                }
                                Some("/exit") => quit = true,
                                Some(other) => execute_command(&mut app, other),
                                None => app.push(
                                    EntryRole::Error,
                                    format!("Unknown command: {text} (/help)"),
                                ),
                            }
                            continue;
                        }
                        app.input_history.push(text.clone());
                        app.history_idx = None;
                        app.bottom_anchor = None; // follow on new send
                        app.running = true;
                        app.phase = "⟡ planning…".to_string();
                        app.turn_started = Some(std::time::Instant::now());
                        app.calls_since_todos = 0;
                        app.reasoning_idx = None;
                        app.stream_finalized = false;
                        app.push(EntryRole::User, text.clone());

                        let ex = executor.clone();
                        let tx2 = tx.clone();
                        let thinking_effort = Some(app.thinking_effort.clone());
                        // Attachments belong to THIS message: take them so the next
                        // prompt starts clean.
                        let attach_paths = std::mem::take(&mut app.attachments);
                        // take ownership of the history for this spawned turn:
                        // swap in an empty vec, hand the real one to the task,
                        // and restore it via the HistoryUpdate event when done
                        let prev_history = std::mem::take(&mut app.conversation_history);
                        tokio::spawn(async move {
                            let (ptx, prx) = mpsc::unbounded_channel();
                            let fwd_tx = tx2.clone();
                            tokio::spawn(forward_progress_events(prx, fwd_tx));
                            let mut history = prev_history;
                            // Read the files on the task, not in the key thread —
                            // base64 of an image is not free. A file that cannot be
                            // read is reported and dropped; the turn still runs.
                            let (attachments, attach_notes) = build_attachments(&attach_paths);
                            if !attach_notes.is_empty() {
                                let _ = tx2.send(TuiEvent::Core(UiEvent::Entry(ChatEntry {
                                    role: EntryRole::Error,
                                    text: format!("attachment: {}", attach_notes.join("; ")),
                                    detail: None,
                                    expanded: false,
                                })));
                            }
                            // The bytes travel WITH this turn: images as image_url
                            // parts, text files inlined. No extra round-trip.
                            let turn_result = ex
                                .run_turn_with_attachments_and_effort(
                                    &text,
                                    &attachments,
                                    &mut history,
                                    Some(ptx),
                                    thinking_effort,
                                )
                                .await;
                            match turn_result {
                                Ok(reply) => {
                                    let _ =
                                        tx2.send(TuiEvent::Core(UiEvent::HistoryUpdate(history)));
                                    let _ = tx2.send(TuiEvent::Core(UiEvent::Entry(ChatEntry {
                                        role: EntryRole::Assistant,
                                        text: reply,
                                        detail: None,
                                        expanded: false,
                                    })));
                                }
                                Err(e) => {
                                    let _ =
                                        tx2.send(TuiEvent::Core(UiEvent::HistoryUpdate(history)));
                                    let _ = tx2.send(TuiEvent::Core(UiEvent::Entry(ChatEntry {
                                        role: EntryRole::Error,
                                        text: e,
                                        detail: None,
                                        expanded: false,
                                    })));
                                }
                            }
                            let _ = tx2.send(TuiEvent::Core(UiEvent::TurnDone));
                        });
                    }
                    KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.palette = Some((String::new(), 0, fuzzy_commands("", 30)));
                    }
                    // Ctrl+O: toggle verbose detail (tool args/reasoning) — session-wide
                    KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.verbose = !app.verbose;
                        let state = if app.verbose { "on" } else { "off" };
                        app.toast = Some((format!(" Details {state} "), 25));
                    }
                    // Ctrl+K toggles all thinking entries; plain `k` remains typeable.
                    KeyCode::Char('k') if thinking_toggle_requested(&key, app.input.is_empty()) => {
                        let any_collapsed = app
                            .entries
                            .iter()
                            .any(|e| e.role == EntryRole::Thinking && !e.expanded);
                        let target = any_collapsed; // expand all if any collapsed, else collapse all
                        for e in app.entries.iter_mut() {
                            if e.role == EntryRole::Thinking && e.detail.is_some() {
                                e.expanded = target;
                                if let Some(full) = &e.detail {
                                    e.text = if target {
                                        format!("▾\n{full}")
                                    } else {
                                        format!("▸ {} words", word_count(full))
                                    };
                                }
                            }
                        }
                        let state = if target { "expanded" } else { "collapsed" };
                        app.toast = Some((format!(" Thinking {state} "), 25));
                    }
                    // In raw mode a pasted line break arrives as Ctrl+J, not Enter
                    // (crossterm #371: raw mode takes \n away from Enter). It must
                    // insert a newline, never send. Also the documented manual
                    // newline key.
                    KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let at = app.cursor.min(app.input.len());
                        app.input.insert(at, '\n');
                        app.cursor = at + 1;
                        app.history_idx = None;
                    }
                    // Ctrl+T: expand the one-line todo strip into the full list.
                    KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.todos_expanded = !app.todos_expanded;
                    }
                    // Ctrl+V: paste from the system clipboard. A copied image
                    // becomes an attachment — a terminal can only deliver text, so
                    // a screenshot has no other way in. Terminals that bind Ctrl+V
                    // themselves (Windows Terminal's default paste) never send this
                    // key; `/paste` is the same thing and always arrives.
                    KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        paste_from_clipboard();
                    }
                    KeyCode::Char(c) => {
                        handle_character(&mut app, c);
                    }
                    KeyCode::Backspace if app.cursor > 0 => {
                        let prev = prev_char_boundary(&app.input, app.cursor);
                        app.input.replace_range(prev..app.cursor, "");
                        app.cursor = prev;
                    }
                    KeyCode::Left if app.cursor > 0 => {
                        app.cursor = prev_char_boundary(&app.input, app.cursor)
                    }
                    KeyCode::Right if app.cursor < app.input.len() => {
                        app.cursor = next_char_boundary(&app.input, app.cursor)
                    }
                    // Alt+Home / Alt+End jump the chat to the bottom / top. They
                    // must sit ABOVE the unguarded arms below, which would
                    // otherwise shadow them — the same shadowing the Ctrl+arrow
                    // comment just below describes.
                    KeyCode::Home if key.modifiers.contains(KeyModifiers::ALT) => {
                        app.bottom_anchor = Some(u16::MAX)
                    }
                    KeyCode::End if key.modifiers.contains(KeyModifiers::ALT) => {
                        app.bottom_anchor = None
                    }
                    KeyCode::Home => app.cursor = 0,
                    KeyCode::End => app.cursor = app.input.len(),
                    // Ctrl+↑/↓ first: an unguarded `KeyCode::Up` arm below would
                    // shadow it, so the documented Ctrl+↑/↓ scroll never ran.
                    KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        scroll_up(&mut app, 1)
                    }
                    KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        scroll_down(&mut app, 1)
                    }
                    // A multi-line prompt: the arrows move within the text.
                    KeyCode::Up if app.input.contains('\n') => cursor_move_line(&mut app, -1),
                    KeyCode::Down if app.input.contains('\n') => cursor_move_line(&mut app, 1),
                    // Empty input: the arrows scroll the chat — that is what a
                    // long history needs. With text typed (or a recall in
                    // progress) they walk the input history instead.
                    KeyCode::Up if up_down_scrolls(&app) => scroll_up(&mut app, 3),
                    KeyCode::Down if app.history_idx.is_none() => scroll_down(&mut app, 3),
                    KeyCode::Up => {
                        if !app.input_history.is_empty() {
                            let len = app.input_history.len();
                            let idx = match app.history_idx {
                                Some(i) => (i + 1).min(len - 1),
                                None => 0,
                            };
                            app.history_idx = Some(idx);
                            let take = len - 1 - idx;
                            app.input = app.input_history[take].clone();
                            app.cursor = app.input.len();
                        }
                    }
                    KeyCode::Down => {
                        if let Some(idx) = app.history_idx {
                            if idx == 0 {
                                app.history_idx = None;
                                app.input.clear();
                                app.cursor = 0;
                            } else {
                                app.history_idx = Some(idx - 1);
                                let take = app.input_history.len() - 1 - (idx - 1);
                                app.input = app.input_history[take].clone();
                                app.cursor = app.input.len();
                            }
                        }
                    }
                    KeyCode::PageUp if key.modifiers.is_empty() => scroll_up(&mut app, 10),
                    KeyCode::PageDown if key.modifiers.is_empty() => scroll_down(&mut app, 10),
                    KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        jump_prompt(&mut app, -1)
                    }
                    KeyCode::Tab => jump_prompt(&mut app, 1),
                    other => {
                        /* unhandled: {:?} */
                        let _ = other;
                    }
                }
            }
            TuiEvent::Core(UiEvent::ConfigReloaded(new_cfg)) => {
                let changes = executor.reload_config(&new_cfg).await;
                if changes.is_empty() {
                    app.push(
                        EntryRole::Status,
                        "⚙ config reloaded (no relevant changes)".to_string(),
                    );
                } else {
                    app.push(
                        EntryRole::Status,
                        format!("⚙ LLM config changed: {changes}"),
                    );
                }
            }
            TuiEvent::Core(UiEvent::StagedBinary) => {
                let Some(new_bin) = rijo_core::selfupd::pending_update() else {
                    app.push(
                        EntryRole::Status,
                        "◇ update signal, but no staging binary.".to_string(),
                    );
                    continue;
                };
                if std::env::var("RIJO_AUTO_SELFUPDATE").as_deref() == Ok("1") {
                    // auto path
                    match rijo_core::selfupd::apply_update(&new_bin) {
                        Ok(()) => {
                            let _ = rijo_core::selfupd::write_resume_marker();
                            app.push(
                                EntryRole::Status,
                                "✅ update applied, restarting with --resume…".to_string(),
                            );
                            // graceful quit; outer loop restart via wrapper
                            quit = true;
                        }
                        Err(e) => {
                            app.push(EntryRole::Error, format!("Update failed: {e}"));
                        }
                    }
                } else {
                    app.push(
                        EntryRole::Status,
                        "🆕 new binary staged — /update or RIJO_AUTO_SELFUPDATE=1".to_string(),
                    );
                }
            }
            TuiEvent::Core(UiEvent::SkillsReloaded) => {
                app.skills_catalog = rijo_core::tools::skills_tool::list_skills(&executor.state());
                app.push(EntryRole::Status, "⚙ skills reloaded".to_string());
            }
            TuiEvent::Core(UiEvent::TodosChanged) => {
                // `todo_list` wrote the file; re-read it for the strip. The counter
                // only resets when the content really changed (see refresh_todos).
                refresh_todos(&mut app);
            }
            TuiEvent::Core(UiEvent::HistoryResults {
                kind,
                query,
                result,
            }) => {
                if let Some(view) = app.history_view.as_mut() {
                    if view.kind == kind && view.query.trim() == query {
                        view.searching = false;
                        match result {
                            Ok(results) => {
                                view.results = results;
                                view.selected = 0;
                                view.error = None;
                            }
                            Err(error) => view.error = Some(error),
                        }
                    }
                }
            }
            TuiEvent::Core(UiEvent::ResumeLoaded { session_id, result }) => {
                match result {
                    Ok(messages) if !messages.is_empty() => {
                        match executor.set_session_id(&session_id) {
                            Ok(()) => {
                                replace_conversation_with_session(&mut app, &messages);
                                app.history_view = None;
                                app.push(
                                EntryRole::Status,
                                format!("Resumed {session_id}. New turns append to this saved session."),
                            );
                            }
                            Err(error) => app.push(EntryRole::Error, error),
                        }
                    }
                    Ok(_) => app.push(
                        EntryRole::Status,
                        format!("No resumable messages found in {session_id}."),
                    ),
                    Err(error) => app.push(
                        EntryRole::Error,
                        format!("Could not resume session: {error}"),
                    ),
                }
            }
            TuiEvent::Core(UiEvent::Ask { questions, tx }) => {
                // Keep the question in the transcript too, then open the overlay.
                // The turn that asked stays blocked until it is answered.
                let text = questions
                    .iter()
                    .map(|q| format!("? {}", q.question))
                    .collect::<Vec<_>>()
                    .join("\n");
                app.push(EntryRole::Question, text);
                app.ask = Some(AskState::new(questions, tx));
            }
            TuiEvent::Paste(text) => {
                // A bracketed paste arrives whole, in one event. Route it to
                // whatever currently owns typed characters: an open overlay first,
                // otherwise the input. The confirm dialog is not a text target.
                let cleaned = clean_paste(&text);
                if cleaned.is_empty() {
                    // Nothing usable (only control characters).
                } else if app.confirm.is_some() {
                    // The dialog only answers y/n; a paste is not an answer.
                } else if let Some((q, sel, items)) = app.palette.as_mut() {
                    q.push_str(&cleaned);
                    *items = fuzzy_commands(q, 30);
                    *sel = 0;
                } else if let Some((q, sel, files)) = app.picker.as_mut() {
                    q.push_str(&cleaned);
                    *files = fuzzy_files(q, 30);
                    *sel = 0;
                } else if let Some(view) = app.history_view.as_mut() {
                    view.query.push_str(&cleaned);
                    view.results.clear();
                    view.selected = 0;
                    view.searching = false;
                    view.error = None;
                } else if let Some((query, selected)) = app.activity_view.as_mut() {
                    query.push_str(&cleaned);
                    *selected = 0;
                } else {
                    apply_paste(&mut app, &cleaned);
                }
            }
            TuiEvent::Clipboard(read) => {
                // Read on a worker thread (see `paste_from_clipboard`); this is the
                // same routing a bracketed paste gets.
                paste_read(&mut app, read);
            }
            TuiEvent::Mouse(m) => {
                // Chat selection + wheel scrolling
                match m.kind {
                    crossterm::event::MouseEventKind::ScrollUp => scroll_up(&mut app, 3),
                    crossterm::event::MouseEventKind::ScrollDown => scroll_down(&mut app, 3),
                    _ => {
                        // A left-click on the status or todo strip toggles that
                        // view instead of starting a chat selection. Only a real
                        // left-button press counts — a move/drag must not toggle.
                        let left_click = matches!(
                            m.kind,
                            crossterm::event::MouseEventKind::Down(MouseButton::Left)
                        );
                        if !(left_click && toggle_status_row(&mut app, m.row)) {
                            if let Some((x, y, w, h)) = app.last_chat_area {
                                let area_r = ratatui::layout::Rect {
                                    x,
                                    y,
                                    width: w,
                                    height: h,
                                };
                                let cfg = tui_panel_select::MouseConfig::default();
                                let action = app.select_panel.handle_mouse(
                                    m,
                                    area_r,
                                    app.last_chat_scroll,
                                    &cfg,
                                );
                                // Copy-on-release feedback. On Windows the crate's own
                                // clipboard is disabled (clip.exe mangles UTF-8 — see
                                // set_clipboard_mode below), so copy the selection here.
                                if matches!(action, tui_panel_select::MouseAction::Copied) {
                                    #[cfg(windows)]
                                    if let Some(text) = app.select_panel.selected_text() {
                                        copy_to_clipboard(&text);
                                    }
                                    app.toast = Some((" copied ".to_string(), 25));
                                }
                                // EDGE AUTO-SCROLL while dragging a selection:
                                // arm continuous scrolling when the drag sits near
                                // the viewport's top/bottom edge; the 80ms timer
                                // tick performs the actual scroll steps so a
                                // resting mouse keeps scrolling smoothly.
                                const EDGE: u16 = 2; // rows at the border that trigger scrolling
                                match m.kind {
                                    crossterm::event::MouseEventKind::Drag(_) => {
                                        if m.row < area_r.y + EDGE {
                                            app.autoscroll = Some(true); // up
                                        } else if m.row
                                            >= area_r.y + area_r.height.saturating_sub(EDGE)
                                        {
                                            app.autoscroll = Some(false); // down
                                        } else {
                                            app.autoscroll = None;
                                        }
                                    }
                                    // Release / click / anything else stops autoscroll
                                    _ => app.autoscroll = None,
                                }
                            }
                        }
                    }
                }
            }
            TuiEvent::Core(UiEvent::Entry(e)) => {
                // Duplicate protection (bidirectional): Entry and StreamDone can
                // arrive in any order over two channels —
                // the second deliverer of the same text is dropped.
                if e.role == EntryRole::Assistant
                    && app
                        .entries
                        .last()
                        .map(|last| last.text == e.text)
                        .unwrap_or(false)
                {
                    // last assistant entry has identical text — duplicate
                } else if app.stream_finalized && e.role == EntryRole::Assistant {
                    app.stream_finalized = false;
                    // skip this entry — StreamDone already pushed the text
                    // into entries.
                } else {
                    app.entries.push(e);
                }
                // keep user scroll position if they scrolled up
            }
            TuiEvent::Core(UiEvent::TurnDone) => {
                app.running = false;
                app.phase.clear();
                app.turn_started = None;
                // final safety: collapse in-flight reasoning regardless of
                // whether the turn produced content/tools (prevents the
                // expanded stream from leaking into the finished chat)
                {
                    fn finalize_reasoning_turn_end(app: &mut App) {
                        if let Some(idx) = app.reasoning_idx.take() {
                            let full = app.reasoning_buf.clone();
                            if full.trim().is_empty() {
                                if idx < app.entries.len()
                                    && app.entries[idx].role == EntryRole::Thinking
                                {
                                    app.entries.remove(idx);
                                }
                            } else if let Some(e) = app.entries.get_mut(idx) {
                                e.text = format!("▸ {} words", word_count(&full));
                                e.detail = Some(full);
                                e.expanded = false;
                            }
                            app.reasoning_buf.clear();
                        } else if !app.reasoning_buf.is_empty() {
                            let full = app.reasoning_buf.clone();
                            app.entries.push(ChatEntry {
                                role: EntryRole::Thinking,
                                text: format!("▸ {} words", word_count(&full)),
                                detail: Some(full),
                                expanded: false,
                            });
                            app.reasoning_buf.clear();
                        }
                    }
                    finalize_reasoning_turn_end(&mut app);
                }
                // If the user did not scroll themselves (anchor=None → follow),
                // they stay in follow mode. If they scrolled, the position stays.
            }
            TuiEvent::Core(UiEvent::Progress(ev)) => {
                // ── Reasoning live buffer: finalize before any content follows ──
                // (new content/tool stream invalidates the in-flight thinking entry)
                fn finalize_reasoning(app: &mut App) {
                    if let Some(idx) = app.reasoning_idx.take() {
                        let full = app.reasoning_buf.clone();
                        if full.trim().is_empty() {
                            // empty reasoning — drop the placeholder entry
                            if idx < app.entries.len()
                                && app.entries[idx].role == EntryRole::Thinking
                            {
                                app.entries.remove(idx);
                            }
                        } else {
                            if let Some(e) = app.entries.get_mut(idx) {
                                e.text = format!("▸ {} words", word_count(&full));
                                e.detail = Some(full.clone());
                                e.role = EntryRole::Thinking;
                                e.expanded = false; // collapse on finalize
                            }
                        }
                        app.reasoning_buf.clear();
                    } else if !app.reasoning_buf.is_empty() {
                        // deltas arrived without a live entry (safety net)
                        let full = app.reasoning_buf.clone();
                        app.entries.push(ChatEntry {
                            role: EntryRole::Thinking,
                            text: format!("▸ {} words", word_count(&full)),
                            detail: Some(full),
                            expanded: false,
                        });
                        app.reasoning_buf.clear();
                    }
                }

                // Persist any mid-turn content (iteration-N text before tool calls)
                // as an assistant entry, so that tool lines sit CHRONOLOGICALLY
                // correct after what has already been said.
                fn finalize_stream_buf(app: &mut App) {
                    if let Some(buf) = app.stream_buf.take() {
                        if !buf.trim().is_empty() {
                            app.entries.push(ChatEntry {
                                role: EntryRole::Assistant,
                                text: buf,
                                detail: None,
                                expanded: false,
                            });
                        }
                    }
                }

                let (role, text) = match ev {
                    rijo_core::executor::ProgressEvent::ToolCall {
                        call_id,
                        tool,
                        args_summary,
                    } => {
                        app.phase = format!("⚙ {}…", tool);
                        app.tool_started = Some(std::time::Instant::now());
                        finalize_reasoning(&mut app);
                        finalize_stream_buf(&mut app);
                        start_tool_entry(&mut app, &call_id, &tool, &args_summary);
                        (EntryRole::Status, String::new())
                    }
                    rijo_core::executor::ProgressEvent::ToolDone {
                        call_id,
                        tool,
                        ok,
                        duration_ms,
                        excerpt,
                        args_json,
                    } => {
                        // after a tool, the LLM is composing the next response
                        app.phase = "⟡ planning…".to_string();
                        app.tool_started = None;
                        app.calls_since_todos = app.calls_since_todos.saturating_add(1);
                        finalize_reasoning(&mut app);
                        finalize_stream_buf(&mut app);
                        finish_tool_entry(
                            &mut app,
                            &call_id,
                            &tool,
                            ok,
                            duration_ms,
                            &excerpt,
                            &args_json,
                        );
                        (EntryRole::Status, String::new())
                    }
                    rijo_core::executor::ProgressEvent::ReasoningDelta { text: delta } => {
                        app.phase = "⟡ thinking…".to_string();
                        app.reasoning_buf.push_str(&delta);
                        if app.reasoning_idx.is_none() {
                            app.reasoning_idx = Some(app.entries.len());
                            // The CURRENT reasoning streams in EXPANDED — watching it
                            // think is the point. It collapses the moment it is
                            // finalized (`finalize_reasoning`), so the transcript is
                            // not left full of old thinking blocks.
                            app.entries.push(ChatEntry {
                                role: EntryRole::Thinking,
                                text: String::new(),
                                detail: None,
                                expanded: true,
                            });
                        }
                        if let Some(idx) = app.reasoning_idx {
                            if let Some(e) = app.entries.get_mut(idx) {
                                if e.expanded {
                                    // live and expanded: show the stream itself
                                    e.text = format!("▾\n{}", app.reasoning_buf);
                                } else {
                                    // the user collapsed it with `k` while it streams
                                    e.text = format!("▸ {} words", word_count(&app.reasoning_buf));
                                }
                                e.detail = Some(app.reasoning_buf.clone());
                            }
                        }
                        (EntryRole::Status, String::new())
                    }
                    rijo_core::executor::ProgressEvent::ToolArgsProgress {
                        tool, preview, ..
                    } => {
                        // Say WHAT is being prepared, not how many argument
                        // characters have arrived — the elapsed time in the row is
                        // the progress signal, a char count is implementation noise.
                        app.phase = if tool.is_empty() {
                            "⚙ preparing a tool call…".to_string()
                        } else if preview.is_empty() {
                            format!("⚙ {tool}…")
                        } else {
                            format!("⚙ {tool} {preview}")
                        };
                        (EntryRole::Status, String::new())
                    }
                    rijo_core::executor::ProgressEvent::Thinking => {
                        (EntryRole::Status, String::new())
                    }
                    rijo_core::executor::ProgressEvent::BrainQuery {
                        hits,
                        top_score,
                        labels,
                        full_text,
                    } => {
                        // Full text means: the embedder was busy, the context is
                        // keyword-only — the user should see that degradation.
                        let mode = if full_text { " · full text" } else { "" };
                        app.phase =
                            format!("⚡ brain · {} hits (top: {:.2}){mode}", hits, top_score);
                        app.entries.push(ChatEntry {
                            role: EntryRole::Tool,
                            // The row already carries the `⚙` origin marker; a second
                            // glyph here was pure noise.
                            text: format!(
                                "brain · {} hits · {:.2}{mode} · {}",
                                hits,
                                top_score,
                                labels.join(" / ")
                            ),
                            detail: Some(labels.join("\n")),
                            expanded: false,
                        });
                        (EntryRole::Tool, String::new())
                    }
                    rijo_core::executor::ProgressEvent::TurnStats {
                        iterations,
                        llm_ms,
                        tool_ms,
                        total_ms,
                        tool_calls,
                        prompt_tokens,
                        last_prompt_tokens,
                        completion_tokens,
                        cache_hit,
                        cache_miss,
                        failures,
                        brain_hits,
                        ttft_ms,
                        context_window,
                    } => {
                        // Keep the measurements in the UI — visible in the status
                        // line once the turn is through (otherwise only in the JSONL).
                        app.session_turns += 1;
                        app.session_prompt_tokens += prompt_tokens as u64;
                        app.session_completion_tokens += completion_tokens as u64;
                        app.last_stats = Some(TurnStatsView {
                            iterations,
                            llm_ms,
                            tool_ms,
                            total_ms,
                            tool_calls,
                            prompt_tokens,
                            last_prompt_tokens,
                            completion_tokens,
                            cache_hit,
                            cache_miss,
                            failures,
                            brain_hits,
                            ttft_ms,
                            context_window,
                        });
                        (EntryRole::Tool, String::new())
                    }
                    rijo_core::executor::ProgressEvent::ContextEstimate { chars, messages } => {
                        app.live_ctx_chars = chars;
                        app.live_ctx_messages = messages;
                        (EntryRole::Tool, String::new())
                    }
                    rijo_core::executor::ProgressEvent::StreamDone => {
                        finalize_reasoning(&mut app);
                        // finalize: move stream_buf into a proper entry —
                        // but NOT if the last assistant entry already
                        // carries the same text (entry event arrived first).
                        if let Some(buf) = app.stream_buf.take() {
                            let dup = app
                                .entries
                                .last()
                                .map(|last| last.text == buf && last.role == EntryRole::Assistant)
                                .unwrap_or(false);
                            if !dup && !buf.trim().is_empty() {
                                app.entries.push(ChatEntry {
                                    role: EntryRole::Assistant,
                                    text: buf,
                                    detail: None,
                                    expanded: false,
                                });
                            }
                        }
                        app.stream_finalized = true;
                        (EntryRole::Status, String::new())
                    }
                    rijo_core::executor::ProgressEvent::StreamDelta { text: delta } => {
                        app.phase = "✎ answering…".to_string();
                        match app.stream_buf.as_mut() {
                            Some(entry) => entry.push_str(&delta),
                            None => {
                                app.stream_buf = Some(delta);
                            }
                        }
                        // follow live ONLY if user hasn't scrolled up

                        (EntryRole::Status, String::new())
                    }
                };
                if !text.is_empty() {
                    app.push(role, text);
                }
            }
            TuiEvent::Core(UiEvent::ConfirmAction { message, tx }) => {
                // Show overlay; block normal input until y/n
                app.confirm = Some((tx, message));
            }
            TuiEvent::Core(UiEvent::HistoryUpdate(h)) => {
                // take()d at turn start; replace with the grown version
                app.conversation_history = h;
            }
        }
    }

    ratatui::restore();
    Ok(())
}

// ─── Drawing ─────────────────────────────────────────────────────────────

fn draw<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    app: &mut App,
) -> Result<(), String> {
    terminal
        .draw(|f| draw_frame(f, app))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Draws the complete frame (chat, status, input, overlays).
/// Separate function so tests can render the same layout into a test backend
/// — otherwise the appearance could not be checked.
/// Height of the measurements row.
///
/// While a turn runs the two rows stay separate — the numbers must not vanish
/// while the activity row changes every frame (GOAL-100 §V.3). Idle there is no
/// live state, so the key measurement joins the state row and this row collapses
/// to zero height. Before the first turn it stays to explain itself.
fn data_row_height(app: &App) -> u16 {
    if app.running || app.last_stats.is_none() {
        1
    } else {
        0
    }
}

fn draw_frame(f: &mut ratatui::Frame, app: &mut App) {
    let app = &mut *app; // reblock for closure; ratatui FnMut ok
                         // The input box grows with its content (capped at MAX_INPUT_LINES) so a long
                         // or multi-line prompt stays visible instead of scrolling off a single line.
    let input_lines = {
        let text_width = f.area().width.saturating_sub(5).max(1) as usize;
        let (rows, _, _) = wrap_input(&app.input, app.cursor, text_width);
        rows.len().clamp(1, MAX_INPUT_LINES)
    };
    let todo_rows = todo_strip_height(app);
    let attach_rows = if app.attachments.is_empty() { 0 } else { 1 };
    let data_rows = data_row_height(app);
    let root = Layout::vertical([
        Constraint::Min(3),                         // chat
        Constraint::Length(1),                      // activity line (spinner, phase, elapsed)
        Constraint::Length(data_rows),              // data line (0 rows when idle — see above)
        Constraint::Length(todo_rows),              // todo strip (0 rows when empty)
        Constraint::Length(attach_rows),            // attached files (0 rows when none)
        Constraint::Length(input_lines as u16 + 2), // input box
    ])
    .split(f.area());

    let cols = ratatui::layout::Layout::horizontal([Constraint::Min(20), Constraint::Length(1)])
        .split(root[0]);
    render_chat(f, cols[0], app);
    render_scrollbar(f, cols[1], app);
    render_toast(f, root[0], app);
    render_picker(f, root[0], app);
    render_palette(f, root[0], app);
    render_tool_explorer(f, root[0], app);
    render_skills_panel(f, root[0], app);
    render_memory_panel(f, root[0], app);
    render_activity_archive_panel(f, root[0], app);
    render_runtime_panel(f, root[0], app);
    render_operations_panel(f, root[0], app);
    render_history_panel(f, root[0], app);
    render_confirm(f, root[0], app);
    render_ask(f, root[0], app);
    // Remember where the bars sit, so a click can toggle their view (below).
    app.last_row_activity = Some(root[1].y);
    app.last_row_todo = (todo_rows > 0).then(|| root[3].y);
    render_activity(f, root[1], app);
    render_data(f, root[2], app);
    render_todos(f, root[3], app);
    render_attachments(f, root[4], app);
    render_input(f, root[5], app);
}

/// One line listing the files attached to the next message. Sits right above the
/// input, next to the todo strip — both are "what is about to happen".
fn render_attachments(f: &mut ratatui::Frame, area: Rect, app: &App) {
    if area.height == 0 || app.attachments.is_empty() {
        return;
    }
    let names: Vec<String> = app
        .attachments
        .iter()
        .map(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.clone())
        })
        .collect();
    let spans = vec![
        Span::styled(
            format!("attached ×{}  ", names.len()),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(names.join(", "), theme::dim()),
        Span::styled("  · /detach".to_string(), theme::dim()),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// How many rows the todo strip takes. `0` means "do not reserve anything": the
/// strip only exists while there is a list.
fn todo_strip_height(app: &App) -> u16 {
    if app.todos.is_empty() {
        0
    } else if app.todos_expanded {
        app.todos.len().clamp(1, 8) as u16
    } else {
        1
    }
}

/// The strip above the input: collapsed it is one line — progress and the task
/// being worked on; Ctrl+T expands it into the full checklist.
fn render_todos(f: &mut ratatui::Frame, area: Rect, app: &App) {
    if area.height == 0 || app.todos.is_empty() {
        return;
    }
    if app.todos_expanded {
        let mut lines: Vec<Line> = Vec::new();
        for t in app.todos.iter().take(area.height as usize) {
            // The item being worked on carries the live spinner, so the checklist
            // itself shows where Rijo Code is right now.
            let (glyph, style) = if t.status == "in_progress" {
                (
                    working_glyph(app),
                    Style::default()
                        .fg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                todo_glyph(&t.status)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{glyph} "), style),
                Span::styled(t.content.clone(), style),
            ]));
        }
        f.render_widget(Paragraph::new(lines), area);
        return;
    }
    let done = app.todos.iter().filter(|t| t.status == "completed").count();
    let cancelled = app.todos.iter().filter(|t| t.status == "cancelled").count();
    let total = app.todos.len();
    let pct = ((done * 100) / total.max(1)) as u32;
    let width = area.width as usize;
    const HINT: &str = "Ctrl+T";

    let mut spans = vec![Span::styled("task ".to_string(), theme::dim())];
    // The list is only as current as the model keeps it: many tool calls since it
    // last CHANGED means the strip shows a step that is no longer the current one.
    // Deliberately independent of `running` — the stale list is just as wrong after
    // the turn ended (real report: it stayed on step 1 after a long run).
    const STALE_AFTER: u32 = 6;
    let stale = app.calls_since_todos >= STALE_AFTER;
    if stale {
        spans.push(Span::styled(
            "(stale) ".to_string(),
            Style::default().fg(theme::WARN),
        ));
    }
    spans.push(Span::styled(
        format!("{} {done}/{total}", gauge(pct)),
        Style::default().fg(theme::OK).add_modifier(Modifier::BOLD),
    ));
    let mut used: usize = spans.iter().map(|s| s.content.chars().count()).sum();

    // Where Rijo Code is right now: the item in progress — or, if the model has
    // not marked one, the next open item (clearly labelled "next", never pretending
    // to be the current one).
    let current = app
        .todos
        .iter()
        .find(|t| t.status == "in_progress")
        .map(|t| (t.content.as_str(), true))
        .or_else(|| {
            app.todos
                .iter()
                .find(|t| t.status == "pending")
                .map(|t| (t.content.as_str(), false))
        });
    if let Some((content, working)) = current {
        // The hint keeps its place at the right edge, so a long task text is
        // ellipsised instead of pushing the affordance off the row.
        let room = width.saturating_sub(used + 2 + HINT.chars().count() + 1 + 4);
        let mut text = content.to_string();
        if text.chars().count() > room {
            text = text.chars().take(room.saturating_sub(1)).collect();
            text.push('…');
        }
        if working {
            used += 4 + text.chars().count();
            spans.push(Span::styled(
                format!("  {} ", working_glyph(app)),
                Style::default().fg(theme::ACCENT),
            ));
            spans.push(Span::styled(
                text,
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            let prefix = "  ▹ next: ";
            used += prefix.chars().count() + text.chars().count();
            spans.push(Span::styled(prefix.to_string(), theme::dim()));
            spans.push(Span::styled(text, theme::dim()));
        }
    }
    // Cancelled items are the one count that is NOT derivable from done/total.
    if cancelled > 0 {
        let s = format!("  · {cancelled} cancelled");
        used += s.chars().count();
        spans.push(Span::styled(s, theme::dim()));
    }
    // Push the hint to the right edge: the strip sits directly above the input, so
    // the affordance belongs at the far side, not buried mid-line.
    let pad = width.saturating_sub(used + HINT.chars().count() + 1);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.push(Span::styled(HINT.to_string(), theme::dim()));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Example state for the look snapshot (not test-only code, so the CLI
/// can use it too): shows all roles, both status-line states and the
/// overlays in one pass.
fn snapshot_app(context_window: u64) -> App {
    let mut app = App::new();
    app.entries.push(ChatEntry {
        role: EntryRole::User,
        text: "start the server on port 8080".into(),
        detail: None,
        expanded: false,
    });
    app.entries.push(ChatEntry {
        role: EntryRole::Thinking,
        // Collapsed by default, like the live path: the header already names the
        // role, so the line carries only the size.
        text: "▸ 9 words".into(),
        detail: Some("The user wants the server started in the background.".into()),
        expanded: false,
    });
    app.entries.push(ChatEntry {
        role: EntryRole::Tool,
        text: "✓ terminal · cargo (bg) · 412ms │ started · proc_1".into(),
        detail: None,
        expanded: false,
    });
    app.entries.push(ChatEntry {
        role: EntryRole::Error,
        text: "✗ read_file · missing.rs · 3ms │ No such file or directory".into(),
        detail: None,
        expanded: false,
    });
    app.entries.push(ChatEntry {
        role: EntryRole::Assistant,
        text: "Running on port 8080.\n\n```bash\ncargo run --release\n```\n\nTo stop:\n\n- `kill proc_1`".into(),
        detail: None,
        expanded: false,
    });
    app.entries.push(ChatEntry {
        role: EntryRole::Status,
        text: "Commands: /help · /clear · /exit".into(),
        detail: None,
        expanded: false,
    });
    // Running-turn look: the status line shows the live context estimate.
    app.live_ctx_chars = 48_000;
    app.live_ctx_messages = 12;
    app.input = "thanks, how do I stop it?".into();
    app.context_window = context_window;
    app.last_stats = Some(TurnStatsView {
        iterations: 2,
        llm_ms: 1100,
        tool_ms: 412,
        total_ms: 1512,
        tool_calls: 1,
        prompt_tokens: 3977,
        last_prompt_tokens: 3977,
        completion_tokens: 33,
        cache_hit: 1024,
        cache_miss: 2953,
        failures: 0,
        brain_hits: 3,
        ttft_ms: 620,
        context_window,
    });
    app
}

/// Renders a complete frame into a test-backend buffer and returns it as
/// text. This makes the appearance checkable without a real terminal —
/// `rijo --tui-snapshot` shows exactly that.
fn render_frame_text(app: &mut App, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut term = ratatui::Terminal::new(backend).expect("TestBackend");
    let _ = term.draw(|f| draw_frame(f, app));
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..height {
        for x in 0..width {
            out.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        out.push('\n');
    }
    out
}

/// All views as (title, frame) — home states, palettes, capability/operations
/// panels, confirm dialog, and question overlay. For `--tui-snapshot` and tests.
pub fn snapshot_frames(
    width: u16,
    height: u16,
    context_window: u64,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();

    let mut app = snapshot_app(context_window);
    out.push(("Idle", render_frame_text(&mut app, width, height)));

    let mut app = snapshot_app(context_window);
    app.running = true;
    app.phase = "✎ writing…".into();
    app.turn_started = Some(std::time::Instant::now());
    out.push(("Running turn", render_frame_text(&mut app, width, height)));

    let mut app = snapshot_app(context_window);
    app.palette = Some(("/".into(), 1, fuzzy_commands("/", 10)));
    app.toast = Some((" Copied to clipboard ".into(), 20));
    out.push((
        "Command palette + toast",
        render_frame_text(&mut app, width, height),
    ));

    let mut app = snapshot_app(context_window);
    app.tool_catalog = build_tool_catalog(&rijo_core::tools::Registry::build_default());
    app.tool_explorer = Some(("github".into(), 0));
    out.push(("Tool directory", render_frame_text(&mut app, width, height)));

    let mut app = snapshot_app(context_window);
    app.skills_catalog = vec![rijo_core::tools::skills_tool::SkillEntry {
        name: "rijo-rust-harness".into(),
        description: "Rust workspace testing and repository conventions".into(),
        category: "development".into(),
        own: true,
    }];
    app.skills_view = Some((String::new(), 0));
    out.push((
        "Skills directory",
        render_frame_text(&mut app, width, height),
    ));

    let mut app = snapshot_app(context_window);
    app.memory_items = vec![MemoryItem {
        target: "user".into(),
        content: "Prefers concise technical explanations.".into(),
    }];
    app.memory_selected = Some(0);
    out.push(("Memory entries", render_frame_text(&mut app, width, height)));

    let mut app = snapshot_app(context_window);
    app.runtime_lines =
        build_runtime_lines(&rijo_core::config::Config::default(), 31, false, false);
    app.runtime_scroll = Some(0);
    out.push((
        "Runtime capabilities",
        render_frame_text(&mut app, width, height),
    ));

    let mut app = snapshot_app(context_window);
    let sample_process = rijo_core::process::Info {
        id: "proc_0000002a".into(),
        pid: 42,
        command: "cargo run --release".into(),
        workdir: ".".into(),
        running: true,
        exit_code: None,
        uptime_ms: 12_000,
        stdout_path: String::new(),
        stderr_path: String::new(),
    };
    app.operations_items = build_operations_items(
        &[sample_process],
        &app.todos,
        &[("job-7".into(), "1h".into())],
    );
    app.operations_selected = Some(0);
    out.push(("Operations", render_frame_text(&mut app, width, height)));

    let mut app = snapshot_app(context_window);
    app.compacted_activity_count = 420;
    app.archived_activity = vec![
        "tool: ✓ read_file · executor.rs · 4ms".into(),
        "status: 2 background processes".into(),
    ];
    app.activity_view = Some(("executor".into(), 0));
    out.push((
        "Archived activity",
        render_frame_text(&mut app, width, height),
    ));

    let mut app = snapshot_app(context_window);
    app.history_view = Some(HistoryView {
        kind: SearchMode::History,
        query: "server".into(),
        selected: 0,
        results: vec![HistoryHit {
            session_id: "sess_20261015".into(),
            role: "assistant".into(),
            timestamp: "2026-10-15T12:00:00Z".into(),
            excerpt: "Started the server and verified its health endpoint.".into(),
        }],
        searching: false,
        error: None,
    });
    out.push(("History search", render_frame_text(&mut app, width, height)));

    let mut app = snapshot_app(context_window);
    app.history_view = Some(HistoryView {
        kind: SearchMode::Brain,
        query: "router".into(),
        selected: 0,
        results: vec![HistoryHit {
            session_id: "src/router.rs".into(),
            role: "code".into(),
            timestamp: "score 0.875".into(),
            excerpt: "The calibrated router chooses a safe tool subset.".into(),
        }],
        searching: false,
        error: None,
    });
    out.push(("Brain search", render_frame_text(&mut app, width, height)));

    let mut app = snapshot_app(context_window);
    let (tx, _rx) = tokio::sync::oneshot::channel::<bool>();
    app.confirm = Some((
        Arc::new(tx),
        "DELETE src/main.rs? This file will be deleted permanently.".into(),
    ));
    out.push(("Confirm dialog", render_frame_text(&mut app, width, height)));

    // A question overlay: what the user sees when Rijo asks and the turn blocks.
    let mut app = snapshot_app(context_window);
    let (tx, _rx) = tokio::sync::oneshot::channel::<Vec<(String, String)>>();
    app.ask = Some(AskState::new(
        vec![
            AskQuestion {
                id: "q1".into(),
                question: "Welcher Befehl schlägt fehl?".into(),
                options: vec![
                    AskOption {
                        label: "cargo build --release".into(),
                        recommended: true,
                    },
                    AskOption {
                        label: "cargo test".into(),
                        recommended: false,
                    },
                ],
                allow_free_text: true,
            },
            AskQuestion {
                id: "q2".into(),
                question: "Darf ich die Lockfile anfassen?".into(),
                options: vec![
                    AskOption {
                        label: "Ja".into(),
                        recommended: false,
                    },
                    AskOption {
                        label: "Nein".into(),
                        recommended: false,
                    },
                ],
                allow_free_text: false,
            },
        ],
        Arc::new(tx),
    ));
    out.push((
        "Question overlay",
        render_frame_text(&mut app, width, height),
    ));

    // Todo strip: collapsed (one line) and expanded (Ctrl+T).
    let todos = || {
        vec![
            TodoView {
                content: "Recherche".into(),
                status: "completed".into(),
            },
            TodoView {
                content: "Fehler isolieren".into(),
                status: "in_progress".into(),
            },
            TodoView {
                content: "Patch schreiben".into(),
                status: "pending".into(),
            },
            TodoView {
                content: "Build + Tests".into(),
                status: "pending".into(),
            },
        ]
    };
    let mut app = snapshot_app(context_window);
    app.todos = todos();
    out.push(("Todo strip", render_frame_text(&mut app, width, height)));
    let mut app = snapshot_app(context_window);
    app.todos = todos();
    app.todos_expanded = true;
    out.push((
        "Todo list (Ctrl+T)",
        render_frame_text(&mut app, width, height),
    ));

    // A long, multi-line prompt: the box grows to MAX_INPUT_LINES and then scrolls.
    let mut app = snapshot_app(context_window);
    app.input = "Bitte analysiere dieses Repository und schreibe eine kurze \
                 Zusammenfassung der Architektur, der wichtigsten Befehle und der \
                 offenen Punkte. \nHalte dich kurz.\n"
        .repeat(3);
    app.cursor = app.input.len();
    out.push((
        "Long input (grown box)",
        render_frame_text(&mut app, width, height),
    ));

    out
}

/// The two-column marker at the start of every transcript line.
///
/// A tool result's own `✓`/`✗` is promoted into this column (see
/// `render_entry`), so the mark is never doubled and the origin column stays the
/// same width for every role — that is what makes the transcript scannable.
fn role_gutter(role: &EntryRole) -> Span<'static> {
    let (glyph, color) = match role {
        EntryRole::User => ("● ", theme::ACCENT),
        EntryRole::Assistant => ("◆ ", theme::ASSISTANT),
        EntryRole::Thinking => ("⟡ ", theme::THINK),
        EntryRole::Tool => ("⚙ ", theme::DIM),
        EntryRole::Question => ("? ", theme::ACCENT),
        EntryRole::Todo => ("✔ ", theme::OK),
        EntryRole::Error => ("✗ ", theme::ERR),
        EntryRole::Status => ("· ", theme::DIM),
    };
    Span::styled(glyph.to_string(), Style::default().fg(color))
}

/// The section rule under a header, in the role's own colour instead of a neutral
/// grey. That is the other half of the "where does this line come from" signal.
fn rule_for(role: &EntryRole) -> Style {
    let color = match role {
        EntryRole::User => theme::ACCENT,
        EntryRole::Assistant => theme::ASSISTANT,
        EntryRole::Thinking => theme::THINK,
        EntryRole::Question => theme::ACCENT,
        EntryRole::Todo => theme::OK,
        EntryRole::Error => theme::ERR,
        EntryRole::Tool | EntryRole::Status => theme::RULE,
    };
    Style::default().fg(color)
}

fn entry_header_span(role: &EntryRole) -> Option<Span<'static>> {
    // Standard Unicode glyphs (👤❯⟡✗) instead of private-use (U+F0xx): PUA only
    // renders with a nerd-font terminal — standard glyphs work everywhere.
    match role {
        EntryRole::User => Some(Span::styled(
            "● you ",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        )),
        EntryRole::Assistant => Some(Span::styled(
            "◆ rijo ",
            Style::default()
                .fg(theme::ASSISTANT)
                .add_modifier(Modifier::BOLD),
        )),
        EntryRole::Tool => None, // tools get compact inline, no header
        EntryRole::Thinking => Some(Span::styled(
            "⟡ thinking ",
            Style::default()
                .fg(theme::THINK)
                .add_modifier(Modifier::BOLD),
        )),
        EntryRole::Error => Some(Span::styled(
            "✗ error ",
            Style::default().fg(theme::ERR).add_modifier(Modifier::BOLD),
        )),
        EntryRole::Status => None,
        EntryRole::Question => Some(Span::styled(
            "? question ",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        )),
        EntryRole::Todo => Some(Span::styled(
            "✔ todos ",
            Style::default().fg(theme::OK).add_modifier(Modifier::BOLD),
        )),
    }
}

fn role_style(role: &EntryRole) -> Style {
    match role {
        EntryRole::User => Style::default().fg(theme::ACCENT),
        EntryRole::Assistant => Style::default(),
        EntryRole::Tool => Style::default().fg(theme::DIM),
        EntryRole::Thinking => Style::default()
            .fg(theme::THINK)
            .add_modifier(Modifier::ITALIC | Modifier::DIM),
        EntryRole::Error => Style::default().fg(theme::ERR),
        EntryRole::Status => Style::default()
            .fg(theme::DIM)
            .add_modifier(Modifier::ITALIC),
        EntryRole::Question => Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::BOLD),
        EntryRole::Todo => Style::default().fg(theme::OK),
    }
}

fn render_chat(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let _ = compact_transient_entries(app, MAX_TRANSIENT_ENTRIES);
    let wrap_width = area.width.saturating_sub(4).max(20) as usize;

    // Markdown-lite: code fences -> DarkGray block, ## headers -> bold, `inline` -> yellow
    let render_entry = |e_role: &EntryRole, e_text: &str, lines: &mut Vec<Line>, is_first: bool| {
        let style = role_style(e_role);
        // A tool result carries its own `✓`/`✗` at the very start. Promote that
        // mark into the gutter (dropping it from the text) so the origin column
        // stays uniform and the mark is not doubled.
        let (mark, body) = match e_text.strip_prefix("✓ ") {
            Some(rest) => (Some(("✓ ", theme::OK)), rest),
            None => match e_text.strip_prefix("✗ ") {
                Some(rest) => (Some(("✗ ", theme::ERR)), rest),
                None => (None, e_text),
            },
        };
        // With a mark there is no header (an inline result is its own line).
        let has_header = mark.is_none() && entry_header_span(e_role).is_some();
        // A tool result reads better UNDER its call than beside it. The entry is
        // "head │ result": split it so the result gets its own indented line with a
        // `│` edge. The head keeps the origin gutter; the tail is dim.
        let (body, result_tail) = match body.split_once(" │ ") {
            Some((head, tail)) => (head, Some(tail.to_string())),
            None => (body, None),
        };
        let gutter: Span<'static> = match mark {
            Some((glyph, color)) => Span::styled(glyph.to_string(), Style::default().fg(color)),
            None => role_gutter(e_role),
        };
        let mut in_code = false;
        let mut code_lang = String::new();
        let mut table_buf: Vec<Vec<String>> = Vec::new();

        for raw_line in body.lines() {
            let first_prefix: Span<'static> = if is_first && !has_header {
                gutter.clone()
            } else {
                Span::raw("  ".to_string())
            };
            let cont_prefix: Span<'static> = Span::raw("  ".to_string());

            if raw_line.starts_with("```") {
                flush_table(&mut table_buf, lines, cont_prefix.clone());
                in_code = !in_code;
                code_lang = raw_line.trim_start_matches('`').trim().to_string();
                // No more empty bar for the fence: just a subtle
                // language label when the language is known.
                if in_code && !code_lang.is_empty() {
                    lines.push(Line::from(vec![
                        cont_prefix.clone(),
                        Span::styled("┃ ".to_string(), Style::default().fg(theme::DIM)),
                        Span::styled(
                            code_lang.clone(),
                            Style::default()
                                .fg(theme::DIM)
                                .add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
                continue;
            }
            if in_code {
                if !code_lang.is_empty() {
                    if let Some(spans) = highlight_code_line(&format!("{raw_line}\n"), &code_lang) {
                        let mut v = vec![cont_prefix.clone(), Span::raw("┃ ".to_string())];
                        v.extend(spans);
                        lines.push(Line::from(v));
                        continue;
                    }
                }
                for wl in textwrap::wrap(raw_line, wrap_width) {
                    lines.push(Line::from(vec![
                        cont_prefix.clone(),
                        Span::styled(format!("┃ {wl}"), Style::default().fg(theme::DIM)),
                    ]));
                }
                continue;
            }
            if raw_line.starts_with("## ")
                || raw_line.starts_with("# ")
                || raw_line.starts_with("### ")
            {
                flush_table(&mut table_buf, lines, cont_prefix.clone());
                for wl in textwrap::wrap(raw_line.trim_start_matches('#').trim(), wrap_width) {
                    lines.push(Line::from(vec![
                        first_prefix.clone(),
                        Span::styled(format!(" {wl}"), style.add_modifier(Modifier::BOLD)),
                    ]));
                }
                continue;
            }
            // table row?
            if raw_line.trim_start().starts_with('|') {
                let cells: Vec<String> = raw_line
                    .trim()
                    .trim_matches('|')
                    .split('|')
                    .map(|c| c.trim().to_string())
                    .collect();
                let is_sep = cells
                    .iter()
                    .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':'));
                if is_sep {
                    continue; // separator row consumed
                }
                table_buf.push(cells);
                continue;
            }
            flush_table(&mut table_buf, lines, cont_prefix.clone());

            for (i, wl) in textwrap::wrap(raw_line, wrap_width).iter().enumerate() {
                let prefix: Span<'static> = if i == 0 {
                    first_prefix.clone()
                } else {
                    cont_prefix.clone()
                };
                // "- point" becomes a bullet marker (purely visual).
                let (bullet, body) = match wl.strip_prefix("- ") {
                    Some(rest) => (
                        Some(Span::styled("• ", Style::default().fg(theme::DIM))),
                        rest.to_string(),
                    ),
                    None => (None, wl.to_string()),
                };
                let mut spans = md_inline_spans(&body, style);
                let mut v = vec![prefix];
                if let Some(b) = bullet {
                    v.push(b);
                }
                v.append(&mut spans);
                lines.push(Line::from(v));
            }
        }
        flush_table(&mut table_buf, lines, Span::raw("  ".to_string()));
        if let Some(tail) = result_tail {
            for line in tail.lines() {
                lines.push(Line::from(vec![
                    Span::raw("  ".to_string()),
                    Span::styled("│ ".to_string(), Style::default().fg(theme::RULE)),
                    Span::styled(line.to_string(), theme::dim()),
                ]));
            }
        }
    };

    app.render_cache.resize_with(app.entries.len(), || None);
    app.render_cache.truncate(app.entries.len());
    for (idx, e) in app.entries.iter().enumerate() {
        let signature = entry_render_signature(e, idx, area.width, app.verbose);
        let cache_hit = app
            .render_cache
            .get(idx)
            .and_then(Option::as_ref)
            .map(|cached| cached.signature == signature)
            .unwrap_or(false);
        if cache_hit {
            continue;
        }
        let mut entry_lines = Vec::new();
        if idx > 0 {
            // Tool lines stack tightly, everything else gets room to breathe. The
            // block boundary marks the header with a fine line — that
            // replaces the earlier heavy full-width rule.
            entry_lines.push(Line::from(""));
        }
        // Header: role name in color, then a fine line up to the edge. A failed
        // tool already carries its own "✗ read_file … │ reason" line, so a
        // "✗ error" header would repeat the mark and the error would show twice.
        // Inline tool results (✓/✗) therefore get no section header.
        let inline_result = e.text.starts_with("✓ ") || e.text.starts_with("✗ ");
        if let Some(header) = entry_header_span(&e.role).filter(|_| !inline_result) {
            let used = header.content.chars().count();
            // Thinking blocks are frequent, and a full-width rule per block turned
            // the transcript into a wall of rules. A short tail keeps the marker
            // without the visual weight.
            let fill = if matches!(e.role, EntryRole::Thinking) {
                6
            } else {
                (area.width as usize).saturating_sub(used + 1)
            };
            let mut v = vec![header];
            if fill > 2 {
                v.push(Span::styled("─".repeat(fill), rule_for(&e.role)));
            }
            entry_lines.push(Line::from(v));
        }
        render_entry(&e.role, &e.text, &mut entry_lines, true);
        // Detail reveals: thinking entries ALREADY carry the text completely in
        // e.text (▾ header + stream body in the same string) — no extra
        // detail dump, otherwise the reasoning shows up twice. Collapsed:
        // only the ▸ line. Verbose (Ctrl+O) only reveals non-thinking details.
        if let Some(d) = &e.detail {
            if e.role != EntryRole::Thinking && app.verbose && !d.trim().is_empty() && d != &e.text
            {
                for dl in d.lines().skip(1) {
                    // only the detail body — first line (= compact summary) is above
                    render_entry(
                        &EntryRole::Status,
                        &format!("  {dl}"),
                        &mut entry_lines,
                        true,
                    );
                }
            }
        }
        // No additional blank line at the end: the in-between line of the next
        // entry already provides the spacing (previously double gaps).
        app.render_cache[idx] = Some(EntryRenderCache {
            signature,
            lines: entry_lines,
        });
    }

    // live streaming buffer (assistant, in progress)
    let mut stream_lines = Vec::new();
    if let Some(buf) = &app.stream_buf {
        render_entry(&EntryRole::Assistant, buf, &mut stream_lines, true);
        stream_lines.push(Line::from(""));
    }

    let total_lines = app
        .render_cache
        .iter()
        .filter_map(Option::as_ref)
        .map(|entry| entry.lines.len())
        .sum::<usize>()
        + stream_lines.len();
    let total_lines = total_lines.max(1);
    let empty_line = Line::from(Span::styled(
        " Type a task to begin.",
        Style::default().fg(theme::DIM),
    ));

    let total = total_lines.min(u16::MAX as usize) as u16;
    let visible = area.height.saturating_sub(1); // minus top border
    let max_scroll = total.saturating_sub(visible);
    CACHED_TOTAL.store(total as usize, std::sync::atomic::Ordering::Relaxed);
    CACHED_VISIBLE.store(visible as usize, std::sync::atomic::Ordering::Relaxed);
    CACHED_MAX_SCROLL.store(max_scroll as usize, std::sync::atomic::Ordering::Relaxed);

    // Clamp bottom_anchor so it never exceeds the current max_scroll
    // (handles the case where entries were removed or the terminal was resized).
    if let Some(a) = app.bottom_anchor {
        app.bottom_anchor = Some(a.min(max_scroll));
    }

    let offset = match app.bottom_anchor {
        Some(anchor) => max_scroll.saturating_sub(anchor),
        None => max_scroll,
    };

    // ── Panel selection wiring (tui-panel-select) ──────────────────────
    // Build the panel content from the SAME rendered lines so that panel
    // rows map 1:1 onto screen rows — what you highlight is what gets
    // copied (headers + separators included, styled spans flattened).
    let inner = internal_with_block(area);
    {
        let signature = selection_content_signature(app, inner.width);
        if app.selection_signature != Some(signature) {
            let mut plain = String::new();
            let mut append_plain = |lines: &[Line<'static>]| {
                for line in lines {
                    for span in line.spans.iter() {
                        plain.push_str(span.content.as_ref());
                    }
                    plain.push('\n');
                }
            };
            for entry in app.render_cache.iter().filter_map(Option::as_ref) {
                append_plain(&entry.lines);
            }
            append_plain(&stream_lines);
            if plain.is_empty() {
                plain.push_str(" Type a task to begin.\n");
            }
            app.select_panel
                .set_content(std::sync::Arc::from(plain.as_str()), inner.width as usize);
            app.selection_signature = Some(signature);
        }
        app.select_panel
            .set_wrap_mode(tui_panel_select::WrapMode::Clip);
        app.last_chat_area = Some((inner.x, inner.y, inner.width, inner.height));
        app.last_chat_scroll = offset;
    }

    let visible_lines = if app.entries.is_empty() && stream_lines.is_empty() {
        vec![empty_line]
    } else {
        collect_visible_chat_lines(
            &app.render_cache,
            &stream_lines,
            offset as usize,
            visible as usize,
        )
    };
    let para = Paragraph::new(visible_lines);

    f.render_widget(para, inner);

    // paint selection highlight over the drawn buffer (REVERSED)
    let hcells = app.select_panel.highlight_cells(inner, offset);
    if !hcells.is_empty() {
        let bwidth = f.area().width;
        let bheight = f.area().height;
        for (row, cfrom, cto) in hcells {
            if row < bheight && cfrom < bwidth {
                for cx in cfrom..cto.min(bwidth) {
                    if let Some(cell) = f
                        .buffer_mut()
                        .cell_mut(ratatui::prelude::Position { x: cx, y: row })
                    {
                        let st = cell.style();
                        cell.set_style(st.add_modifier(ratatui::style::Modifier::REVERSED));
                    }
                }
            }
        }
    }
    // No more continuous rule at the top edge: the role headers with a
    // fine line structure the history, the full-width rule looked like a
    // table border and visually cut through the scrollbar.
}

/// Compact number: 42100 -> "42.1k".
fn human_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 10_000 {
        // 4400 -> "4.4k" (one decimal while it still carries information)
        format!("{:.1}k", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        // 128000 -> "128k", not "128.0k"
        format!("{:.0}k", n as f64 / 1000.0)
    } else if n < 10_000_000 {
        // 1048576 -> "1.0M" (a 1M window must not read as "1000k")
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{:.0}M", n as f64 / 1_000_000.0)
    }
}

/// Context fill of the last request. `window == 0` means the model's window is
/// unknown (config `llm.context_window`): then only the prompt size is shown —
/// inventing a denominator would turn a number into a lie.
///
/// Thresholds: <60% dim, 60-84% amber, >=85% red+bold. Red is the point where
/// pruning/compaction starts to bite, so it should be visible before it happens.
fn ctx_segment(used: u32, window: u64, estimated: bool) -> Option<(String, Style)> {
    if used == 0 {
        return None;
    }
    let mark = if estimated { "≈" } else { "" };
    if window == 0 {
        return Some((
            format!("context {mark}{}", human_tokens(used as u64)),
            theme::dim(),
        ));
    }
    let pct = ((used as f64 / window as f64) * 100.0).round() as u32;
    let style = if pct >= 85 {
        Style::default().fg(theme::ERR).add_modifier(Modifier::BOLD)
    } else if pct >= 60 {
        Style::default().fg(theme::WARN)
    } else {
        theme::dim()
    };
    // "of" instead of "/" so the pair does not read like a fraction, and the gauge
    // only when at least one cell is filled — five empty cells are just noise.
    let bar = gauge(pct);
    let fill = if bar.contains('▰') {
        format!(" {bar}")
    } else {
        String::new()
    };
    Some((
        format!(
            "context {mark}{} of {} ({pct}%){fill}",
            human_tokens(used as u64),
            human_tokens(window)
        ),
        style,
    ))
}

/// Time to first token as its own segment: it is the latency the user feels,
/// and it is NOT the same as throughput — a slow TTFT reads as "it hangs" while
/// tok/s looks fine afterwards. Amber above 5 s.
fn ttft_segment(ttft_ms: u64) -> Option<(String, Style)> {
    if ttft_ms == 0 {
        return None;
    }
    let style = if ttft_ms >= 5000 {
        Style::default().fg(theme::WARN)
    } else {
        theme::dim()
    };
    let human = if ttft_ms >= 1000 {
        format!("{:.1}s", ttft_ms as f64 / 1000.0)
    } else {
        format!("{ttft_ms}ms")
    };
    Some((format!("· first {human}"), style))
}

/// Session totals: many small turns add up, and a single turn hides that.
fn session_segment(app: &App) -> Option<(String, Style)> {
    if app.session_turns == 0 {
        return None;
    }
    let total = app.session_prompt_tokens + app.session_completion_tokens;
    Some((
        format!(
            "· sess {} turn(s) · {} tok",
            app.session_turns,
            human_tokens(total)
        ),
        theme::dim(),
    ))
}

/// Running background processes: the user has to see that something is still
/// alive (otherwise a forgotten server is only noticed when the port is busy).
fn bg_segment() -> Option<(String, Style)> {
    let running = rijo_core::process::list()
        .iter()
        .filter(|p| p.running)
        .count();
    (running > 0).then(|| (format!("· bg {running}"), Style::default().fg(theme::WARN)))
}

/// A five-cell gauge for a percentage — the fill as a shape, so "how full" reads
/// at a glance next to the number. The segment's colour tells the same story
/// (DIM below 60 %, amber 60–84 %, red from 85 %); the gauge does not add colour
/// of its own, it stays part of the value it belongs to.
fn gauge(pct: u32) -> String {
    const CELLS: usize = 5;
    let filled = (pct.min(100) as usize * CELLS + 50) / 100;
    (0..CELLS)
        .map(|i| if i < filled { '▰' } else { '▱' })
        .collect()
}

/// Which status bar a screen row belongs to (set by `draw_frame`).
#[derive(Debug, PartialEq, Eq)]
enum StatusRow {
    Activity,
    Todo,
}

fn status_row_target(app: &App, row: u16) -> Option<StatusRow> {
    if app.last_row_todo == Some(row) {
        Some(StatusRow::Todo)
    } else if app.last_row_activity == Some(row) {
        Some(StatusRow::Activity)
    } else {
        None
    }
}

/// A left click on a status bar toggles *that* bar's view: the activity row the
/// full breakdown (same as Ctrl+O), the todo strip the checklist (same as Ctrl+T).
/// Returns `true` when the click was consumed, so it does not also start a chat
/// selection. Pure enough to unit-test without the event loop.
fn toggle_status_row(app: &mut App, row: u16) -> bool {
    match status_row_target(app, row) {
        Some(StatusRow::Todo) => {
            app.todos_expanded = !app.todos_expanded;
            true
        }
        Some(StatusRow::Activity) => {
            app.verbose = !app.verbose;
            true
        }
        None => false,
    }
}

/// Activity row (directly above the data row): what Rijo Code is doing RIGHT NOW —
/// spinner, phase, tool, live rate, elapsed. Idle it is the state plus the UI
/// hints, because that is the only thing "happening".
fn activity_segments(app: &App) -> Vec<Seg> {
    let mut segs: Vec<Seg> = Vec::new();

    if app.running {
        segs.push(Seg::new(
            format!(" {}", spinner_glyph(app.spinner_frame)),
            Style::default()
                .fg(theme::WARN)
                .add_modifier(Modifier::BOLD),
            10,
        ));
        // live phase: what Rijo Code is doing right now (planning/tool/answering/thinking)
        let mut p = if app.phase.is_empty() {
            "generating…".to_string()
        } else {
            app.phase.clone()
        };
        if p.starts_with("✎") {
            if let Some(sb) = &app.stream_buf {
                let n = sb.chars().count();
                let secs = app.turn_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                // The live number is an ESTIMATE: the provider only reports its
                // token count at the end of the stream. ~4 chars per token, and
                // the "≈" says so instead of pretending precision.
                let per_s = if secs > 0 {
                    (n as u64 / 4) / secs.max(1)
                } else {
                    0
                };
                p = format!("{p} {n} ch · ≈{per_s} tok/s");
            }
        } else if p.starts_with("⚙") {
            if let Some(t0) = app.tool_started {
                let s = t0.elapsed().as_secs_f32();
                if s >= 1.0 {
                    p = format!("{p} {:.1}s", s);
                }
            }
        }
        segs.push(Seg::new(p, Style::default().fg(theme::WARN), 20));
        if let Some(t0) = app.turn_started {
            let secs = t0.elapsed().as_secs();
            let dur = if secs >= 60 {
                format!("{}m{}s", secs / 60, secs % 60)
            } else {
                format!("{secs}s")
            };
            segs.push(Seg::new(format!("· {dur}"), theme::dim(), 40));
        }
        if let Some(seg) = bg_segment() {
            segs.push(Seg::new(seg.0, seg.1, 50));
        }
        // The abort hint is the escape hatch while a turn runs: right-aligned so
        // it never interrupts the live state, and gone when nothing runs.
        segs.push(Seg::hint("Ctrl+C aborts", theme::dim(), 90));
    } else {
        segs.push(Seg::new(
            " ● ready".to_string(),
            Style::default().fg(theme::OK).add_modifier(Modifier::BOLD),
            10,
        ));
        // Idle there is no live state, so the key measurement joins this row and
        // the data row collapses (see `data_row_height`).
        if let Some(st) = &app.last_stats {
            if let Some(seg) = ctx_segment(st.last_prompt_tokens as u32, st.context_window, false) {
                segs.push(Seg::new(format!("· {}", seg.0), seg.1, 30));
            }
        }
        if let Some(seg) = bg_segment() {
            segs.push(Seg::new(seg.0, seg.1, 50));
        }
    }

    if app.bottom_anchor.is_some() {
        segs.push(Seg::new(
            format!("· ↑ {}% · End=follow", scroll_pct(app)),
            theme::dim(),
            60,
        ));
    }
    // The compact/full toggle is an idle affordance: while a turn runs the abort
    // hint is the one that matters, and the row should stay calm.
    if !app.running {
        let hint = if app.verbose {
            "Ctrl+O=compact"
        } else {
            "Ctrl+O=details"
        };
        segs.push(Seg::hint(
            hint.to_string(),
            if app.verbose {
                Style::default()
                    .fg(theme::DIM)
                    .add_modifier(Modifier::REVERSED)
            } else {
                theme::dim()
            },
            90,
        ));
    }
    segs
}

/// Data row (directly above the input box): the measurements of the last turn —
/// context fill, first-token latency, throughput, cache, session totals.
///
/// It stays visible WHILE a turn runs (the activity row above is the one that
/// changes), so the numbers can be read at any moment instead of flashing by.
/// While running the context part shows the live ESTIMATE of the request being
/// sent, marked with "≈"; everything after it belongs to the last finished turn.
fn data_segments(app: &App) -> Vec<Seg> {
    let mut segs: Vec<Seg> = Vec::new();

    // Context: live estimate while running (there is no finished request yet,
    // and on the first turn no previous number either), otherwise the real count.
    let window = if app.context_window > 0 {
        app.context_window
    } else {
        app.last_stats
            .as_ref()
            .map(|st| st.context_window)
            .unwrap_or(0)
    };
    if app.running && app.live_ctx_chars > 0 {
        let est_tokens = (app.live_ctx_chars / 4) as u32;
        if let Some(seg) = ctx_segment(est_tokens, window, true) {
            segs.push(Seg::new(seg.0, seg.1, 10));
        }
        // A second live value: the estimate alone made the row look empty.
        if app.live_ctx_messages > 0 {
            segs.push(Seg::new(
                format!("· {} msg", app.live_ctx_messages),
                theme::dim(),
                30,
            ));
        }
    } else if let Some(st) = &app.last_stats {
        if let Some(seg) = ctx_segment(st.last_prompt_tokens as u32, st.context_window, false) {
            segs.push(Seg::new(seg.0, seg.1, 10));
        }
    }

    if let Some(st) = &app.last_stats {
        if let Some(seg) = ttft_segment(st.ttft_ms) {
            segs.push(Seg::new(seg.0, seg.1, 20));
        }
        // Compact by default: the few numbers worth a permanent glance. Ctrl+O
        // reveals the full breakdown — the same toggle that expands a tool call.
        let line = if app.verbose {
            st.line()
        } else {
            st.compact_line()
        };
        segs.push(Seg::new(format!("· {line}"), theme::dim(), 40));
    } else if app.live_ctx_chars == 0 {
        // No measurement yet: say so instead of leaving the row blank (an empty
        // row looks like a rendering bug and makes the layout jump).
        segs.push(Seg::new(
            "· no measurements yet — the first turn fills this row",
            theme::dim(),
            10,
        ));
    }

    // Session totals are the least urgent value: full view only.
    if app.verbose {
        if let Some(seg) = session_segment(app) {
            segs.push(Seg::new(seg.0, seg.1, 60));
        }
    }
    segs
}

/// One segment of a status row.
///
/// `priority` is the DROP order: on a narrow row the renderer removes the highest
/// number first, so hints (90) vanish long before the context value (10). `right`
/// pins a segment to the right edge — the key hints, which then do not interrupt
/// the flowing state text.
struct Seg {
    text: String,
    style: Style,
    priority: u8,
    right: bool,
}

impl Seg {
    fn new(text: impl Into<String>, style: Style, priority: u8) -> Self {
        Self {
            text: text.into(),
            style,
            priority,
            right: false,
        }
    }

    /// A hint pinned to the right edge of the row.
    fn hint(text: impl Into<String>, style: Style, priority: u8) -> Self {
        Self {
            text: text.into(),
            style,
            priority,
            right: true,
        }
    }
}

/// Emits a status row.
///
/// Flowing segments are chosen by priority (low numbers survive, high numbers are
/// dropped whole) and then drawn in their original order, so a value is never cut
/// in the middle — dropping a whole item beats showing "Cache 2…", which reads
/// like 2 %. A dropped tail is marked with a lone "…". `right` segments are pinned
/// to the right edge and drawn only when they fit completely (never as
/// "Ctrl+O=det…").
fn render_segments(f: &mut ratatui::Frame, area: Rect, segs: Vec<Seg>) {
    let width = area.width as usize;

    let right: Vec<&Seg> = segs.iter().filter(|s| s.right).collect();
    let right_text: String = right
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("  ");
    let right_len = right_text.chars().count();

    // Priority selection over the WHOLE width; the hints are only added afterwards
    // and only if the row still has room. That way a long live state pushes the
    // hint out, never the other way round.
    let mut order: Vec<usize> = (0..segs.len()).filter(|i| !segs[*i].right).collect();
    order.sort_by_key(|i| (segs[*i].priority, *i));
    let mut keep: Vec<usize> = Vec::new();
    let mut used = 0usize;
    for i in order {
        let len = segs[i].text.chars().count();
        let cost = len + usize::from(!keep.is_empty());
        if used + cost <= width {
            used += cost;
            keep.push(i);
        }
    }
    keep.sort_unstable();
    let dropped = segs.iter().filter(|s| !s.right).count() - keep.len();

    let mut spans: Vec<Span> = Vec::new();
    for (n, i) in keep.iter().enumerate() {
        let seg = &segs[*i];
        let mut text = seg.text.clone();
        if n == 0 {
            // A leading separator goes with the segment before it (which may have
            // been dropped, leaving a dangling "· ").
            if let Some(rest) = text.strip_prefix("· ") {
                text = rest.to_string();
            }
        } else {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(text, seg.style));
    }
    if dropped > 0 && !keep.is_empty() && used + 2 <= width {
        spans.push(Span::raw(" "));
        spans.push(Span::styled("…", theme::dim()));
        used += 2;
    }

    if !right.is_empty() && used + 1 + right_len <= width {
        let pad = width - used - right_len;
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(right_text, right[0].style));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_activity(f: &mut ratatui::Frame, area: Rect, app: &App) {
    render_segments(f, area, activity_segments(app));
}

fn render_data(f: &mut ratatui::Frame, area: Rect, app: &App) {
    render_segments(f, area, data_segments(app));
}

/// Percentage scrolled up (0=bottom, 100=top) from cached metrics.
fn scroll_pct(app: &App) -> u16 {
    let max_scroll = CACHED_MAX_SCROLL.load(std::sync::atomic::Ordering::Relaxed) as u16;
    let anchor = app.bottom_anchor.unwrap_or(0);
    if max_scroll == 0 {
        0
    } else {
        ((anchor as u32 * 100) / max_scroll as u32).min(100) as u16
    }
}

// ─── Style language ──────────────────────────────────────────────────────
//
// One place for colors and emphasis so the interface stays consistent
// (previously Color:: literals were scattered across all the render
// functions). Purely visual — no function depends on it.
mod theme {
    use ratatui::style::{Color, Modifier, Style};

    // ── Palette ──────────────────────────────────────────────────────────
    //
    // Truecolor (there was already one RGB literal here), muted instead of the
    // saturated 16-colour primaries: a UI that is mostly text reads better with
    // accents a step below full red / green / cyan. The values are the widely
    // used "One Dark" family.

    /// Focus/selection and the user's own role: input border, selected row, ● you.
    pub const ACCENT: Color = Color::Rgb(97, 175, 239);
    /// The assistant's own role (▲ rijo): a teal next to the blue accent, so the
    /// two voices differ without one of them looking like a warning.
    pub const ASSISTANT: Color = Color::Rgb(86, 182, 194);
    /// Success (✓).
    pub const OK: Color = Color::Rgb(152, 195, 121);
    /// Error (✗).
    pub const ERR: Color = Color::Rgb(224, 108, 117);
    /// Warning / live turn: spinner, elevated latency, background processes.
    pub const WARN: Color = Color::Rgb(229, 192, 123);
    /// Reasoning / thinking.
    pub const THINK: Color = Color::Rgb(198, 120, 221);
    /// Secondary text: timings, hints, labels, code. Brighter than the old
    /// DarkGray so it stays readable, still clearly behind the content.
    pub const DIM: Color = Color::Rgb(120, 128, 143);
    /// Section-header rule: one step above the background, below any text.
    pub const RULE: Color = Color::Rgb(62, 70, 86);

    pub fn dim() -> Style {
        Style::default().fg(DIM)
    }

    /// Label in overlay titles.
    pub fn overlay_title() -> Style {
        Style::default().fg(DIM)
    }

    /// Selected row in lists: filled instead of just bold — recognizable
    /// at a glance even when the terminal font is thin.
    pub fn selected() -> Style {
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD)
    }

    /// Border: active vs. inactive.
    pub fn border_active() -> Style {
        Style::default().fg(ACCENT)
    }

    pub fn border_idle() -> Style {
        Style::default().fg(DIM)
    }

    /// Border shape of the overlays (rounded instead of square).
    pub fn rounded() -> ratatui::widgets::BorderType {
        ratatui::widgets::BorderType::Rounded
    }
}

/// How many display lines the input box may grow to before it starts scrolling.
/// Beyond this the box would eat the whole chat on a short terminal.
const MAX_INPUT_LINES: usize = 8;

fn render_input(f: &mut ratatui::Frame, area: Rect, app: &App) {
    // The `❯` prompt already says "type here", so a title would only repeat it.
    // Keep one for the state that is *not* obvious from the prompt: a running
    // turn (the input is then locked).
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::rounded());
    block = if app.running {
        block
            .border_style(theme::border_idle())
            .title(Span::styled(" running… ", theme::overlay_title()))
    } else {
        block.border_style(theme::border_active())
    };

    // Columns left for the text: two borders plus the three-column indent
    // (" ❯ " on the first row, three spaces on the continuation rows).
    let text_width = area.width.saturating_sub(5).max(1) as usize;
    let content_h = area.height.saturating_sub(2) as usize;
    let (rows, cur_row, cur_col) = wrap_input(&app.input, app.cursor, text_width);
    // Vertical scroll: keep the cursor row visible when the box is at its cap.
    let scroll = if content_h > 0 && cur_row >= content_h {
        cur_row + 1 - content_h
    } else {
        0
    };

    let mut lines: Vec<Line> = Vec::new();
    for (i, row) in rows.iter().enumerate().skip(scroll).take(content_h) {
        let mut spans: Vec<Span> = vec![if i == 0 {
            Span::styled(" ❯ ", Style::default().fg(theme::ACCENT))
        } else {
            Span::raw("   ")
        }];
        spans.extend(row_spans(row, (i == cur_row).then_some(cur_col)));
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(" ❯ ", Style::default().fg(theme::ACCENT)),
            Span::styled(
                " ".to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ),
        ]));
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// One display row as spans, with the cursor character reversed (or a reversed
/// space when the cursor is at the row's end). Char-indexed: multi-byte input
/// must not be split mid-sequence.
fn row_spans(row: &str, cursor_col: Option<usize>) -> Vec<Span<'static>> {
    let Some(col) = cursor_col else {
        return vec![Span::raw(row.to_string())];
    };
    let chars: Vec<char> = row.chars().collect();
    let (before, rest) = if col <= chars.len() {
        (
            chars[..col].iter().collect::<String>(),
            chars[col..].iter().collect::<String>(),
        )
    } else {
        (row.to_string(), String::new())
    };
    let mut spans = vec![Span::raw(before)];
    if let Some(cur) = rest.chars().next() {
        spans.push(Span::styled(
            cur.to_string(),
            Style::default().add_modifier(Modifier::REVERSED),
        ));
        spans.push(Span::raw(rest[cur.len_utf8()..].to_string()));
    } else {
        spans.push(Span::styled(
            " ".to_string(),
            Style::default().add_modifier(Modifier::REVERSED),
        ));
    }
    spans
}

/// One-line summary of the reasoning text (first sentence-ish, ≤ 90 chars).
#[allow(dead_code)] // superseded by the streaming reasoning view; kept for now
fn short_reasoning(s: &str) -> String {
    let one = s.replace('\n', " ");
    let chars: Vec<char> = one.chars().collect();
    if chars.len() <= 90 {
        one.trim().to_string()
    } else {
        let head: String = chars[..87].iter().collect();
        format!("{}…", head.trim())
    }
}

/// Full detail block for a finished tool call (Ctrl+O verbose).
fn entry_full_detail(tool: &str, ok: bool, duration_ms: u64, excerpt: &str) -> String {
    let dur = if duration_ms >= 1000 {
        format!("{:.1}s", duration_ms as f64 / 1000.0)
    } else {
        format!("{}ms", duration_ms)
    };
    // bounded excerpt: keep at most ~30 lines
    let mut lines: Vec<&str> = excerpt.lines().collect();
    let truncated = lines.len() > 30;
    lines.truncate(30);
    let mut out = format!("{} {} · {}", if ok { "✓" } else { "✗" }, tool, dur);
    out.push('\n');
    out.push_str(&lines.join("\n"));
    if truncated {
        out.push_str("\n  … (truncated)");
    }
    out
}

/// Smart one-line excerpt: human value instead of raw JSON blob.
/// Covers ALL registered tools — if a tool is missing, the UI shows raw
/// JSON blobs (that is exactly what happened after the expansion to 26 tools).
fn smart_excerpt(tool: &str, args_json: &str, excerpt: &str) -> String {
    if excerpt.is_empty() {
        return String::new();
    }
    let parsed: serde_json::Value =
        serde_json::from_str(excerpt).unwrap_or(serde_json::Value::Null);
    // error case: keep the raw message (that IS the useful info)
    if let Some(err) = parsed.get("error").and_then(|e| e.as_str()) {
        if !err.is_empty() {
            let e: String = err.chars().take(90).collect();
            return format!(" │ {e}");
        }
    }
    let args: serde_json::Value =
        serde_json::from_str(args_json).unwrap_or(serde_json::Value::Null);
    let arr_len =
        |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_array()).map(|a| a.len());
    let num = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_u64());
    let text = |v: &serde_json::Value, k: &str| {
        v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
    };
    let head_line = |s: &str, n: usize| -> String {
        s.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .chars()
            .take(n)
            .collect()
    };

    let summary: Option<String> = match tool {
        "read_file" => num(&parsed, "total_lines")
            .map(|n| format!("{n} lines"))
            .or_else(|| {
                parsed
                    .get("content")
                    .and_then(|c| c.as_str())
                    .map(|c| format!("{} lines", c.lines().count()))
            }),
        "write_file" => num(&parsed, "bytes_written")
            .map(|b| format!("{b} Bytes"))
            .or(Some("ok".to_string())),
        "delete_file" => Some("deleted".to_string()),
        "patch" => num(&parsed, "changes")
            .map(|n| format!("{n} changes"))
            .or(Some("applied".to_string())),
        "search_files" => num(&parsed, "count")
            .map(|n| format!("{n} matches"))
            .or_else(|| {
                parsed
                    .get("matches")
                    .and_then(|m| m.as_str())
                    .map(|m| format!("{} matches", m.lines().count()))
            }),
        "terminal" => {
            // background start: show the id so process_manage matches it
            if args
                .get("background")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
            {
                parsed
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .map(|id| format!("started · {}", rijo_core::summarize::short_id(id)))
                    .or(Some("background".to_string()))
            } else if !text(&parsed, "error").trim().is_empty() {
                // Make the error line visible. Previously a blocked
                // action showed "ok" because only `output` was read — the user
                // saw a ✗ without any reason.
                Some(
                    text(&parsed, "error")
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .chars()
                        .take(60)
                        .collect(),
                )
            } else {
                let out = text(&parsed, "output");
                let last = out
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("");
                if last.trim().is_empty() {
                    Some("ok".to_string())
                } else {
                    Some(last.trim().chars().take(60).collect())
                }
            }
        }
        // context answers with plain text as well (status/recall/show).
        "context" => Some(head_line(excerpt, 70)),
        // github returns plain text ("rijoda/rijo-code  [private]"), not JSON, so
        // the first line is the informative one (repo/PR/run name).
        "github" => Some(head_line(excerpt, 60)),
        "process_manage" => {
            let action = text(&args, "action");
            match action.as_str() {
                "list" => num(&parsed, "count").map(|n| format!("{n} process(es)")),
                "poll" => {
                    let out = text(&parsed, "new_output");
                    let err = text(&parsed, "new_stderr");
                    if out.trim().is_empty() && err.trim().is_empty() {
                        Some("no new output".to_string())
                    } else {
                        Some(format!(
                            "{} new line(s) ({} err)",
                            out.lines().count(),
                            err.lines().count()
                        ))
                    }
                }
                "log" => num(&parsed, "total_lines").map(|n| format!("{n} lines total")),
                "wait" | "kill" => parsed
                    .pointer("/process/running")
                    .and_then(|r| r.as_bool())
                    .map(|running| {
                        if running {
                            "still running".to_string()
                        } else {
                            let code = parsed
                                .pointer("/process/exit_code")
                                .and_then(|c| c.as_i64())
                                .unwrap_or(-1);
                            format!("terminated (exit {code})")
                        }
                    }),
                "write" | "submit" => Some("stdin sent".to_string()),
                "close" => Some("stdin closed".to_string()),
                _ => None,
            }
        }
        "execute_code" => {
            let out = text(&parsed, "output");
            if out.trim().is_empty() {
                Some("ok".to_string())
            } else {
                Some(head_line(&out, 60))
            }
        }
        "skills_list" => arr_len(&parsed, "skills").map(|n| format!("{n} Skills")),
        "skill_view" => parsed
            .get("content")
            .and_then(|c| c.as_str())
            .map(|c| format!("{} lines", c.lines().count())),
        "skill_manage" => parsed
            .get("path")
            .and_then(|p| p.as_str())
            .map(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.to_string())
            })
            .or(Some("ok".to_string())),
        "memory" => num(&parsed, "current_chars")
            .map(|c| format!("{c} chars saved"))
            .or(Some("ok".to_string())),
        "todo_list" => parsed.get("todos").and_then(|v| v.as_array()).map(|list| {
            // Speak the resulting state, not just a count: "3 entries" in the call
            // next to "6 task(s)" in the result was two numbers and no meaning.
            let n = list.len();
            let done = list
                .iter()
                .filter(|t| t["status"].as_str() == Some("completed"))
                .count();
            let running = list
                .iter()
                .filter(|t| t["status"].as_str() == Some("in_progress"))
                .count();
            let mut s = format!("{n} tasks");
            if done > 0 {
                s.push_str(&format!(" · {done} done"));
            }
            if running > 0 {
                s.push_str(&format!(" · {running} running"));
            }
            s
        }),
        "session_search" => arr_len(&parsed, "results").map(|n| format!("{n} session(s)")),
        "cronjob_manage" => arr_len(&parsed, "jobs")
            .map(|n| format!("{n} job(s)"))
            .or(Some("ok".to_string())),
        "delegate_task" => arr_len(&parsed, "results")
            .map(|n| format!("{n} subagent(s)"))
            .or_else(|| parsed.get("result").map(|_| "done".to_string()))
            .or(Some("ok".to_string())),
        "git" => parsed
            .get("branch")
            .and_then(|v| v.as_str())
            .map(|branch| {
                let counts = &parsed["counts"];
                let changed = ["staged", "unstaged", "untracked"]
                    .iter()
                    .filter_map(|key| counts.get(*key).and_then(|v| v.as_u64()))
                    .sum::<u64>();
                if changed == 0 {
                    format!("{branch} · clean")
                } else {
                    format!("{branch} · {changed} changed")
                }
            })
            .or_else(|| arr_len(&parsed, "commits").map(|n| format!("{n} commit(s)")))
            .or_else(|| num(&parsed, "count").map(|n| format!("{n} files")))
            .or_else(|| arr_len(&parsed, "branches").map(|n| format!("{n} branch(es)")))
            .or_else(|| parsed.get("diff").map(|_| "diff".to_string()))
            .or(Some("ok".to_string())),
        "clarify" => parsed
            .get("answer")
            .and_then(|a| a.as_str())
            .map(|a| head_line(a, 60))
            .or(Some("asked".to_string())),
        "web_search" => {
            let arr = parsed
                .pointer("/data/web")
                .and_then(|w| w.as_array())
                .or_else(|| parsed.get("web").and_then(|w| w.as_array()));
            arr.map(|a| {
                let prov = parsed
                    .get("provider")
                    .and_then(|p| p.as_str())
                    .unwrap_or("?");
                format!("{} results ({prov})", a.len())
            })
        }
        "web_extract" => arr_len(&parsed, "results")
            .map(|n| format!("{n} page(s)"))
            .or_else(|| {
                parsed
                    .get("content")
                    .and_then(|c| c.as_str())
                    .map(|c| format!("{} chars", c.chars().count()))
            }),
        "arxiv" => {
            if let Some(results) = parsed.get("results").and_then(|v| v.as_array()) {
                let title = results
                    .first()
                    .and_then(|paper| paper.get("title"))
                    .and_then(|v| v.as_str())
                    .map(|s| head_line(s, 55))
                    .unwrap_or_default();
                Some(if title.is_empty() {
                    format!("{} paper(s)", results.len())
                } else {
                    format!("{} paper(s) · {title}", results.len())
                })
            } else {
                let action = text(&args, "action");
                let paper = text(&parsed, "paper");
                let citations = num(&parsed["data"], "citationCount");
                match (action.as_str(), paper.is_empty(), citations) {
                    (_, false, Some(count)) => Some(format!("{paper} · {count} citations")),
                    (_, false, None) => Some(paper),
                    _ => Some(format!("arXiv {action}")),
                }
            }
        }
        "brain_search" => parsed.get("hits").and_then(|v| v.as_array()).map(|hits| {
            if let Some(top) = hits.first() {
                let source = top.get("source").and_then(|v| v.as_str()).unwrap_or("");
                let label = top.get("label").and_then(|v| v.as_str()).unwrap_or("");
                let target = if source.is_empty() { label } else { source };
                if target.is_empty() {
                    format!("{} hit(s)", hits.len())
                } else {
                    format!("{} hit(s) · {}", hits.len(), head_line(target, 48))
                }
            } else {
                "0 hit(s)".to_string()
            }
        }),
        "brain_outline" => num(&parsed, "file_count").map(|files| {
            let symbols = parsed
                .get("symbol_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            format!("{files} file(s) · {symbols} symbol(s)")
        }),
        "mcp" => {
            let action = text(&args, "action");
            match action.as_str() {
                "list_servers" => arr_len(&parsed, "servers")
                    .map(|n| format!("{n} server(s)"))
                    .or(Some("servers listed".to_string())),
                "list_tools" => arr_len(&parsed, "tools")
                    .map(|n| format!("{n} tools"))
                    .or(Some("tools listed".to_string())),
                "call" => {
                    let result = text(&parsed, "result");
                    let server = text(&parsed, "server");
                    let remote_tool = text(&parsed, "tool");
                    let source = format!("{server}/{remote_tool}");
                    Some(if result.is_empty() {
                        source
                    } else {
                        format!("{source} · {}", head_line(&result, 45))
                    })
                }
                _ => Some("MCP action complete".to_string()),
            }
        }
        "browser_navigate" => {
            let title = text(&parsed, "title");
            if title.is_empty() || title.starts_with('{') {
                Some("navigated".to_string())
            } else {
                Some(title.chars().take(50).collect())
            }
        }
        "browser_snapshot" => parsed
            .get("content")
            .and_then(|c| c.as_str())
            .map(|c| format!("{} chars", c.chars().count())),
        "browser_click" => Some("clicked".to_string()),
        "browser_type" => Some("typed".to_string()),
        "browser_scroll" => Some("scrolled".to_string()),
        "browser_dev" => {
            let action = text(&args, "action");
            match action.as_str() {
                "screenshot" | "pdf" => {
                    let bytes = num(&parsed, "bytes").unwrap_or(0);
                    Some(format!(
                        "{} · {}",
                        if action == "pdf" { "pdf" } else { "png" },
                        bytes
                    ))
                }
                "evaluate" => parsed
                    .get("result")
                    .map(|value| {
                        let text = match value.as_str() {
                            Some(text) => text.to_string(),
                            None => value.to_string(),
                        };
                        head_line(&text, 60)
                    })
                    .filter(|text| !text.is_empty()),
                "console" => {
                    let count = arr_len(&parsed, "entries").unwrap_or(0);
                    Some(format!(
                        "{count} console {}",
                        if count == 1 { "entry" } else { "entries" }
                    ))
                }
                "network" => {
                    let count = arr_len(&parsed, "entries").unwrap_or(0);
                    Some(format!(
                        "{count} request{}",
                        if count == 1 { "" } else { "s" }
                    ))
                }
                "viewport" => Some(format!(
                    "{}×{}",
                    num(&parsed, "width").unwrap_or(0),
                    num(&parsed, "height").unwrap_or(0)
                )),
                "tabs" => {
                    let count = arr_len(&parsed, "tabs").unwrap_or(0);
                    Some(format!("{count} tab{}", if count == 1 { "" } else { "s" }))
                }
                "tab_open" => Some(format!(
                    "opened {}",
                    head_line(&text(&parsed, "opened"), 20)
                )),
                "tab_select" => Some("selected".to_string()),
                "tab_close" => Some("closed".to_string()),
                "cookies" => {
                    let count = arr_len(&parsed, "cookies").unwrap_or(0);
                    Some(format!(
                        "{count} cookie{}",
                        if count == 1 { "" } else { "s" }
                    ))
                }
                "cookie_set" => Some("cookie set".to_string()),
                "cookie_delete" => Some("cookie deleted".to_string()),
                _ => Some("devtools".to_string()),
            }
        }
        "vision_analyze" => parsed
            .get("content")
            .and_then(|c| c.as_str())
            .or_else(|| parsed.get("answer").and_then(|a| a.as_str()))
            .map(|c| head_line(c, 70)),
        "text_to_speech" => parsed.get("backend").and_then(|b| b.as_str()).map(|b| {
            let base = parsed
                .get("audio_path")
                .and_then(|p| p.as_str())
                .and_then(|p| {
                    std::path::Path::new(p)
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                })
                .unwrap_or_default();
            format!("{b} · {base}")
        }),
        box_tool if box_tool.starts_with("box_") => {
            let status = text(&parsed, "status");
            let count = num(&parsed, "count");
            let output = text(&parsed, "output");
            let content = text(&parsed, "content");
            if !status.is_empty() {
                Some(status)
            } else if let Some(count) = count {
                Some(format!("{count} item(s)"))
            } else if !output.is_empty() {
                Some(head_line(&output, 55))
            } else if !content.is_empty() {
                Some(format!("{} chars", content.chars().count()))
            } else {
                Some("ok".to_string())
            }
        }
        _ => None,
    };
    match summary {
        Some(s) if !s.is_empty() => format!(" │ {s}"),
        // fallback: plain-text head, without JSON braces/quotes
        _ => {
            let head: String = excerpt.chars().take(60).collect();
            let clean = head.replace(['{', '}', '"'], "").replace('\n', " ");
            format!(" │ {}", clean.trim())
        }
    }
}

/// Resolve a human-readable keyword (path, command) from raw tool args.
/// Used in the compact tool-done line: "✓ read_file · src/main.rs · 12ms"
/// Word count for the collapsed thinking header.
fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

fn internal_with_block(area: Rect) -> Rect {
    Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height.saturating_sub(1),
    }
}

/// How close two key events must be for the first to count as "a character typed
/// right before this Enter". A human cannot press two keys within 15 ms; a paste
/// delivers its characters back to back. Kept SMALL on purpose: a wider window
/// made a fast typist's Enter look like a pasted line break, so the prompt never
/// sent.
const BURST_GAP: std::time::Duration = std::time::Duration::from_millis(15);

/// How long an Enter waits to see whether more input is already waiting behind
/// it. A pasted line break is part of a stream, so the next character is there;
/// a human pressing Enter to send stops typing. Generated wide: the whole point
/// is that a pasted prompt can never be split, and 80 ms is imperceptible on a
/// deliberate send.
const PASTE_LOOKAHEAD: std::time::Duration = std::time::Duration::from_millis(25);

/// Whether a key that arrived `gap` after the previous one belongs to a paste
/// burst rather than being an isolated keypress. Extracted so the rule is tested:
/// a lone Enter must still SEND, only a burst Enter is a pasted newline.
fn is_paste_burst(gap: Option<std::time::Duration>) -> bool {
    matches!(gap, Some(g) if g < BURST_GAP)
}

/// What an unmodified Enter means. It SENDS only when nothing else is going on:
/// no key just before it (a burst) and nothing already waiting behind it. A paste
/// delivers its line breaks inside a stream, so a human pressing Enter to send
/// produces neither signal — and this rule is the whole reason a pasted prompt
/// cannot be split.
fn enter_is_pasted_newline(burst: bool, more_pending: bool) -> bool {
    burst || more_pending
}

/// Control-free form of a paste: `\r\n`/`\r` become `\n`, tabs become spaces and
/// every other control character is dropped — a paste must not be able to smuggle
/// an escape sequence into the buffer and repaint the screen. Line breaks are
/// KEPT: the input renders as multiple lines.
fn clean_paste(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

/// Insert a pasted block at the cursor as ONE edit.
fn apply_paste(app: &mut App, text: &str) {
    let cleaned = clean_paste(text);
    if cleaned.is_empty() {
        return;
    }
    let at = app.cursor.min(app.input.len());
    app.input.insert_str(at, &cleaned);
    app.cursor = at + cleaned.len();
    // A paste is new content, not a recall: leave history navigation.
    app.history_idx = None;
}

/// Paste the system clipboard into whatever currently has focus.
///
/// Reading the clipboard spawns an OS tool (PowerShell is ~300 ms warm, more under
/// load), so the read runs on a worker thread and comes back as a [`TuiEvent::Clipboard`].
/// A paste must not freeze the UI, and it only ever happens on a real paste —
/// never on a timer or a redraw.
fn paste_from_clipboard() {
    let Some(tx) = EVENT_TX.get().cloned() else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || {
            let _ = tx.send(TuiEvent::Clipboard(crate::clipboard::read()));
        });
}

/// The decision half of [`paste_from_clipboard`], split out so a test can drive
/// it without an OS clipboard.
fn paste_read(app: &mut App, read: crate::clipboard::ClipboardRead) {
    use crate::clipboard::ClipboardRead;
    match read {
        ClipboardRead::Image(path) => {
            let shown = path.display().to_string();
            app.attachments.push(shown.clone());
            app.push(
                EntryRole::Status,
                format!("attached from clipboard: {shown}"),
            );
        }
        ClipboardRead::Text(text) => {
            // The confirm dialog answers y/n; a paste is not an answer.
            if app.confirm.is_some() {
                return;
            }
            let cleaned = clean_paste(&text);
            if cleaned.is_empty() {
                return;
            }
            if let Some((q, sel, items)) = app.palette.as_mut() {
                q.push_str(&cleaned);
                *items = fuzzy_commands(q, 30);
                *sel = 0;
                return;
            }
            if let Some((q, sel, files)) = app.picker.as_mut() {
                q.push_str(&cleaned);
                *files = fuzzy_files(q, 30);
                *sel = 0;
                return;
            }
            // A copied or dragged-in FILE arrives as its path. When that path is an
            // image, the user means the picture, not the string — attach it instead
            // of typing a path into the prompt.
            let candidate = cleaned.trim().trim_matches('"');
            if rijo_core::llm::is_image_path(candidate) && std::path::Path::new(candidate).is_file()
            {
                app.attachments.push(candidate.to_string());
                app.push(
                    EntryRole::Status,
                    format!("attached from clipboard: {candidate}"),
                );
                return;
            }
            apply_paste(app, &cleaned);
        }
        ClipboardRead::Empty => {
            app.toast = Some((" clipboard: nothing to paste ".to_string(), 25));
        }
    }
}

/// Lay the input out for display: hard-wrapped at `width` characters, honouring
/// explicit line breaks. Returns the display rows plus the cursor's `(row,
/// column)` in that layout.
///
/// Wrapping is per CHARACTER: an editor has to break a long word, not reflow it,
/// or the cursor would jump around while typing.
fn wrap_input(input: &str, cursor: usize, width: usize) -> (Vec<String>, usize, usize) {
    let width = width.max(1);
    let cursor = cursor.min(input.len());
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let mut cursor_pos: Option<(usize, usize)> = None;

    for (byte_idx, ch) in input.char_indices() {
        // Wrap before placing the char, so a cursor sitting exactly on a boundary
        // lands at the start of the next row rather than past the end of the old.
        if row.chars().count() >= width {
            rows.push(std::mem::take(&mut row));
        }
        if byte_idx == cursor {
            cursor_pos = Some((rows.len(), row.chars().count()));
        }
        if ch == '\n' {
            rows.push(std::mem::take(&mut row));
        } else {
            row.push(ch);
        }
    }
    if cursor_pos.is_none() {
        // Cursor at the end. A full last row means it belongs to the next row.
        if row.chars().count() >= width {
            rows.push(std::mem::take(&mut row));
        }
        cursor_pos = Some((rows.len(), row.chars().count()));
    }
    rows.push(row);
    let (r, c) = cursor_pos.unwrap_or((0, 0));
    (rows, r, c)
}

/// Move the cursor one LOGICAL line up (`dir < 0`) or down (`dir > 0`), keeping
/// the column where it fits. Used by ↑/↓ while the prompt spans several lines.
fn cursor_move_line(app: &mut App, dir: i32) {
    let text = &app.input;
    let cursor = app.cursor.min(text.len());
    let line_start = text[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = text[line_start..cursor].chars().count();
    let target = if dir < 0 {
        if line_start == 0 {
            return;
        }
        let prev_end = line_start - 1; // the '\n' itself
        let prev_start = text[..prev_end].rfind('\n').map(|i| i + 1).unwrap_or(0);
        prev_start + char_offset(&text[prev_start..prev_end], col)
    } else {
        let Some(line_end) = text[cursor..].find('\n').map(|i| cursor + i) else {
            return; // already on the last line
        };
        let next_start = line_end + 1;
        let next_end = text[next_start..]
            .find('\n')
            .map(|i| next_start + i)
            .unwrap_or(text.len());
        next_start + char_offset(&text[next_start..next_end], col)
    };
    app.cursor = target;
    app.history_idx = None;
}

/// The byte offset of the `col`-th character of `line` (clamped to its length).
fn char_offset(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(line.len())
}

/// Whether an unmodified ↑/↓ moves the chat instead of the input history: true
/// when there is nothing to edit and no recall in progress. A user with a long
/// history reaches for the arrows to scroll; a user mid-edit reaches for them to
/// recall a previous prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CtrlCAction {
    Quit,
    Cancel,
}

fn thinking_toggle_requested(key: &KeyEvent, input_empty: bool) -> bool {
    input_empty && key.code == KeyCode::Char('k') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn tick_requires_redraw(app: &App, todos_changed: bool, toast_expired: bool) -> bool {
    app.running || app.autoscroll.is_some() || todos_changed || toast_expired
}

fn tick_interval(app: &App) -> std::time::Duration {
    let ms = if app.autoscroll.is_some() {
        30
    } else if app.running || app.toast.is_some() {
        80
    } else {
        750
    };
    std::time::Duration::from_millis(ms)
}

fn ctrl_c_action(running: bool) -> CtrlCAction {
    if running {
        CtrlCAction::Cancel
    } else {
        CtrlCAction::Quit
    }
}

fn handle_character(app: &mut App, c: char) {
    app.input.insert(app.cursor, c);
    app.cursor += c.len_utf8();
    app.history_idx = None;
    // Open the file picker on a fresh '@'.
    if c == '@' && app.picker.is_none() {
        let results = fuzzy_files("", 30);
        app.picker = Some((String::new(), 0, results));
    }
}

fn up_down_scrolls(app: &App) -> bool {
    app.input.is_empty() && app.history_idx.is_none()
}

/// Scroll up n lines: anchor grows (viewport bottom moves up).
fn scroll_up(app: &mut App, n: u16) {
    let max_scroll = CACHED_MAX_SCROLL.load(std::sync::atomic::Ordering::Relaxed) as u16;
    let base = app.bottom_anchor.unwrap_or(0);
    app.bottom_anchor = Some(base.saturating_add(n).min(max_scroll.max(1)));
}

/// Scroll down n lines; reaching bottom returns to follow mode.
fn scroll_down(app: &mut App, n: u16) {
    if let Some(a) = app.bottom_anchor {
        let rest = a.saturating_sub(n);
        app.bottom_anchor = if rest == 0 { None } else { Some(rest) };
    }
}

fn prev_char_boundary(s: &str, i: usize) -> usize {
    s[..i]
        .char_indices()
        .next_back()
        .map(|(j, _)| j)
        .unwrap_or(0)
}
fn next_char_boundary(s: &str, i: usize) -> usize {
    s[i..]
        .char_indices()
        .nth(1)
        .map(|(j, _)| i + j)
        .unwrap_or(s.len())
}

// ─── Code syntax highlighting (syntect) ─────────────────────────────────

use std::sync::LazyLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Highlight a single code line with syntect + base16-ocean.dark → ratatui Spans.
/// Falls back to None when lang unknown.
fn highlight_code_line(line: &str, lang: &str) -> Option<Vec<Span<'static>>> {
    let syntax = SYNTAX_SET.find_syntax_by_token(lang)?;
    let theme = &THEME_SET.themes["base16-ocean.dark"];
    let mut hl = HighlightLines::new(syntax, theme);
    let ranges = hl.highlight_line(line, &SYNTAX_SET).ok()?;
    let mut spans = Vec::new();
    for (style, text) in ranges {
        let c = style.foreground;
        spans.push(Span::styled(
            text.trim_end_matches('\n').to_string(),
            Style::default().fg(Color::Rgb(c.r, c.g, c.b)),
        ));
    }
    if spans.is_empty() {
        None
    } else {
        Some(spans)
    }
}

// ─── Minimap (right gutter, one tick per user prompt) ────────────────────

/// Wrapped-line offset of the start of each USER entry.
/// Approximates wrap width — good enough for tick placement.
fn prompt_line_offsets(entries: &[ChatEntry], wrap_width: usize) -> Vec<u16> {
    let mut offsets: Vec<u16> = Vec::new();
    let mut acc: u16 = 0;
    for e in entries {
        if e.role == EntryRole::User {
            offsets.push(acc);
        }
        let body = e
            .text
            .lines()
            .map(|l| {
                if l.starts_with("```") {
                    1u16
                } else {
                    textwrap::wrap(l, wrap_width).len() as u16
                }
            })
            .sum::<u16>();
        acc += body.max(1) + 1; // + blank separator line
    }
    offsets
}

/// Right-edge scrollbar for the chat area. Uses ratatui's native Scrollbar.
fn render_scrollbar(f: &mut ratatui::Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height < 2 {
        return;
    }
    let total = CACHED_TOTAL.load(std::sync::atomic::Ordering::Relaxed) as u16;
    let visible = CACHED_VISIBLE.load(std::sync::atomic::Ordering::Relaxed) as u16;
    if total <= visible {
        return; // nothing to scroll
    }
    let max_scroll = total.saturating_sub(visible);
    let offset = match app.bottom_anchor {
        Some(anchor) => max_scroll.saturating_sub(anchor),
        None => max_scroll,
    };
    let content_len = total as usize;
    let lengths = content_len;
    let mut state = ratatui::widgets::ScrollbarState::new(lengths).position(offset as usize);
    // Render inside the content region (skip the chat's top-border row) so
    // the thumb aligns 1:1 with content rows; no arrow heads.
    let track = internal_with_block(area);
    f.render_stateful_widget(
        ratatui::widgets::Scrollbar::default()
            .orientation(ratatui::widgets::ScrollbarOrientation::VerticalRight)
            .thumb_symbol("▐")
            .track_symbol(Some("│"))
            .begin_symbol(None)
            .end_symbol(None),
        track,
        &mut state,
    );
}

/// Total wrapped-line count (must mirror render_chat computation).
#[allow(dead_code)] // used by the not-yet-wired minimap below
fn total_wrapped_lines(entries: &[ChatEntry], wrap_width: usize) -> u16 {
    let mut acc: u16 = 0;
    for e in entries {
        let body = e
            .text
            .lines()
            .map(|l| {
                if l.starts_with("```") {
                    1u16
                } else {
                    textwrap::wrap(l, wrap_width).len() as u16
                }
            })
            .sum::<u16>();
        acc += body.max(1) + 1;
    }
    acc
}

/// Right-edge minimap: tick per user prompt at proportional height.
#[allow(dead_code)] // parked feature: not wired into `draw()` yet
fn render_minimap(f: &mut ratatui::Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let height = area.height;
    let wrap_w = 80usize; // approximate, consistent with offsets fn
    let prompt_lines = prompt_line_offsets(&app.entries, wrap_w);
    let total = total_wrapped_lines(&app.entries, wrap_w).max(1);

    let mut ticks: Vec<(u16, bool)> = Vec::new();
    for (i, off) in prompt_lines.iter().enumerate() {
        let y = ((f64::from(*off) / f64::from(total)) * f64::from(height)) as u16;
        ticks.push((
            y.min(height.saturating_sub(1)),
            i as i32 == app.selected_prompt,
        ));
    }

    let mut out_lines: Vec<Line> = Vec::new();
    for y in 0..height {
        let sel = ticks.iter().any(|(ty, s)| *ty == y && *s);
        let tick = ticks.iter().any(|(ty, _)| *ty == y);
        let (ch, style) = if sel {
            (
                "▉",
                Style::default()
                    .fg(theme::WARN)
                    .add_modifier(Modifier::BOLD),
            )
        } else if tick {
            ("▍", Style::default().fg(theme::ACCENT))
        } else {
            ("│", Style::default().fg(theme::RULE))
        };
        out_lines.push(Line::from(Span::styled(ch.to_string(), style)));
    }
    f.render_widget(Paragraph::new(out_lines), area);
}

/// Jump to previous/next user prompt; sets scroll so that prompt is visible,
/// and marks it selected (yellow in minimap). dir = +1 next, -1 prev.
fn jump_prompt(app: &mut App, dir: i32) {
    if app.entries.is_empty() {
        return;
    }
    let prompt_idx: Vec<usize> = app
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.role == EntryRole::User)
        .map(|(i, _)| i)
        .collect();
    if prompt_idx.is_empty() {
        return;
    }
    // currently selected index in prompt_idx terms
    let cur = if app.selected_prompt < 0 {
        if dir > 0 {
            0usize
        } else {
            prompt_idx.len() - 1
        }
    } else {
        let pos = prompt_idx
            .iter()
            .position(|&i| i as i32 == app.selected_prompt)
            .unwrap_or(0);
        (pos as i32 + dir).rem_euclid(prompt_idx.len() as i32) as usize
    };
    app.selected_prompt = prompt_idx[cur] as i32;

    // scroll so that entry appears ~2 lines from the top
    let wrap_w = 80usize;
    let line_off = prompt_line_offsets(&app.entries, wrap_w)[cur];
    let max_scroll = CACHED_MAX_SCROLL.load(std::sync::atomic::Ordering::Relaxed) as u16;
    let top = line_off.saturating_sub(2);
    app.bottom_anchor = Some(max_scroll.saturating_sub(top));
}

// ─── Markdown-lite inline helpers ────────────────────────────────────────

/// Render `**bold**` and `` `code` `` inline spans within a wrapped line.
fn md_inline_spans(line: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut rest = line;
    let mut plain = String::new();
    while !rest.is_empty() {
        // emphasis
        if rest.starts_with("**") {
            if let Some(end) = rest[2..].find("**") {
                if !plain.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut plain)));
                }
                spans.push(Span::styled(
                    rest[2..2 + end].to_string(),
                    base.add_modifier(Modifier::BOLD),
                ));
                rest = &rest[2 + end + 2..];
                continue;
            }
        }
        if rest.starts_with('`') {
            if let Some(end) = rest[1..].find('`') {
                if !plain.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut plain)));
                }
                spans.push(Span::styled(
                    rest[1..1 + end].to_string(),
                    base.fg(theme::ACCENT),
                ));
                rest = &rest[1 + end + 1..];
                continue;
            }
        }
        if let Some(c) = rest.chars().next() {
            plain.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    if !plain.is_empty() {
        spans.push(Span::raw(plain));
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

/// Flush buffered table rows as a rendered grid into `lines`.
fn flush_table(buf: &mut Vec<Vec<String>>, lines: &mut Vec<Line>, cont_prefix: Span<'static>) {
    if buf.is_empty() {
        return;
    }
    let ncols = buf.iter().map(|r| r.len()).max().unwrap_or(0);
    let plain_len = |s: &str| -> usize { s.replace("**", "").replace('`', "").chars().count() };
    let mut widths = vec![3usize; ncols];
    for row in buf.iter() {
        for (j, cell) in row.iter().enumerate() {
            widths[j] = widths[j].max(plain_len(cell).max(3));
        }
    }
    // cap total width at ~72 cols
    let total: usize = widths.iter().sum::<usize>() + (ncols + 1) * 3;
    if total > 72 {
        let scale = (72.0 as usize as f32) / (total as f32);
        for w in widths.iter_mut() {
            *w = (((*w as f32) * scale) as usize).max(3);
        }
    }

    let seg = |l: &str, r: &str, m: &str| -> String {
        let mut s = String::from(l);
        for (j, w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(*w + 2));
            if j + 1 < widths.len() {
                s.push_str(m);
            }
        }
        s.push_str(r);
        s
    };
    let top = seg("┌", "┐", "┬");
    let mid = seg("├", "┤", "┼");
    let bot = seg("└", "┘", "┴");

    let _border_style = Style::default().fg(theme::RULE);
    let row_str = |row: &[String], bold: bool| -> String {
        let mut s = String::from("│");
        for (j, w) in widths.iter().enumerate() {
            let cell = row.get(j).cloned().unwrap_or_default();
            let plain = cell.replace("**", "").replace('`', "");
            let pad = w.saturating_sub(plain.chars().count());
            s.push_str(&format!(" {}{} │", plain, " ".repeat(pad)));
        }
        let _ = bold;
        s
    };

    // simple sequential rendering
    for (i, row) in buf.iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                cont_prefix.clone(),
                Span::raw(top.clone()),
            ]));
            lines.push(Line::from(vec![
                cont_prefix.clone(),
                Span::styled(
                    row_str(row, true),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(vec![
                cont_prefix.clone(),
                Span::raw(mid.clone()),
            ]));
        } else {
            lines.push(Line::from(vec![
                cont_prefix.clone(),
                Span::raw(row_str(row, false)),
            ]));
        }
    }
    lines.push(Line::from(vec![cont_prefix, Span::raw(bot)]));
    buf.clear();
}

// ─── Toast + clipboard (OSC 52) ─────────────────────────────────────────

fn render_toast(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some((text, _)) = &app.toast else { return };
    let w = (text.chars().count() as u16)
        .max(20)
        .min(area.width.saturating_sub(2));
    let x = area.x + area.width.saturating_sub(w + 2);
    let y = area.y + area.height.saturating_sub(2);
    let rect = Rect {
        x,
        y,
        width: w,
        height: 1,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let line = Line::from(Span::styled(
        format!(" {text} "),
        Style::default()
            .fg(Color::Black)
            .bg(theme::ACCENT)
            .add_modifier(Modifier::BOLD),
    ));
    f.render_widget(Paragraph::new(line), rect);
}

/// Copy text to the system clipboard.
///
/// **Windows first.** `clip.exe` (what the selection crate would otherwise use)
/// reads stdin in the *console code page*, not UTF-8, so the chat's `·`, `→`, `✓`
/// and every umlaut came back as mojibake — reportedly "only weird characters".
/// PowerShell is asked to decode stdin as UTF-8 explicitly and store real UTF-16.
///
/// Then OSC 52, which works over SSH/tmux and in most modern terminals and is the
/// only path on a bare headless shell. Inside tmux the sequence is wrapped in a
/// DCS passthrough (ESC doubled), or tmux swallows it.
fn copy_to_clipboard(text: &str) {
    if text.is_empty() {
        return;
    }
    #[cfg(windows)]
    if copy_windows(text) {
        return;
    }
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = if std::env::var_os("TMUX").is_some() {
        let inner = format!("\x1b]52;c;{b64}\x07").replace('\x1b', "\x1b\x1b");
        format!("\x1bPtmux;{inner}\x1b\\")
    } else {
        format!("\x1b]52;c;{b64}\x07")
    };
    use std::io::Write;
    let _ = std::io::stdout().write_all(seq.as_bytes());
    let _ = std::io::stdout().flush();
}

/// Windows clipboard via PowerShell: set the console input encoding to UTF-8,
/// read all of stdin, hand it to `Set-Clipboard`. Returns `false` when
/// PowerShell is unavailable or failed, so the caller can fall back to OSC 52.
#[cfg(windows)]
fn copy_windows(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let script = "[Console]::InputEncoding=[System.Text.UTF8Encoding]::new();\
                  $t=[Console]::In.ReadToEnd(); Set-Clipboard -Value $t";
    let mut child = match Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

// ─── @file fuzzy picker ──────────────────────────────────────────────────

/// Collect project files for the picker (git-aware, fallback to walk).
/// Cached module-level so we don't rescan on every keystroke.
fn collect_files() -> Vec<String> {
    use std::sync::atomic::AtomicU64;
    use std::sync::OnceLock;
    static FILES: OnceLock<Vec<String>> = OnceLock::new();
    static CWD_SIG: AtomicU64 = AtomicU64::new(0);
    let _ = CWD_SIG;
    FILES
        .get_or_init(|| {
            let out = std::process::Command::new("git")
                .args(["ls-files", "--cached", "--others", "--exclude-standard"])
                .output()
                .ok()
                .filter(|o| o.status.success() && !o.stdout.is_empty())
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .map(String::from)
                        .collect::<Vec<_>>()
                });
            out.unwrap_or_else(|| {
                // naive walk, capped
                let mut v = Vec::new();
                fn walk(dir: &std::path::Path, v: &mut Vec<String>, depth: u32) {
                    if depth > 5 || v.len() > 5000 {
                        return;
                    }
                    if let Ok(rd) = std::fs::read_dir(dir) {
                        for e in rd.flatten() {
                            let p = e.path();
                            let name = e.file_name().to_string_lossy().to_string();
                            if name.starts_with('.') || name == "node_modules" || name == "target" {
                                continue;
                            }
                            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                walk(&e.path(), v, depth + 1);
                            } else {
                                if let Ok(s) =
                                    p.strip_prefix(std::env::current_dir().unwrap_or_default())
                                {
                                    v.push(s.to_string_lossy().to_string());
                                }
                            }
                        }
                    }
                }
                walk(&std::env::current_dir().unwrap_or_default(), &mut v, 0);
                v.sort();
                v
            })
        })
        .clone()
}

/// Fuzzy-match files against a query using nucleo, return top N.
fn fuzzy_files(query: &str, n: usize) -> Vec<String> {
    let files = collect_files();
    if query.is_empty() {
        return files.into_iter().take(n).collect();
    }
    let mut matcher = nucleo::Matcher::new(nucleo::Config::DEFAULT);
    let mut candidates: Vec<(u32, &String)> = Vec::new();
    {
        let pattern = nucleo::pattern::Pattern::parse(
            query,
            nucleo::pattern::CaseMatching::Smart,
            nucleo::pattern::Normalization::Smart,
        );
        let mut scratch = Vec::new();
        for f in &files {
            let haystack = nucleo::Utf32Str::new(f.as_str(), &mut scratch);
            if let Some(score) = pattern.score(haystack, &mut matcher) {
                candidates.push((score, f));
            }
        }
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));
    candidates
        .into_iter()
        .take(n)
        .map(|(_, f)| f.clone())
        .collect()
}

fn render_picker(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some((q, sel, files)) = &app.picker else {
        return;
    };
    let h = 15u16
        .min(area.height.saturating_sub(2))
        .min(files.len().saturating_add(2) as u16 + 1);
    let w = area.width.saturating_sub(4).min(60);
    let x = area.x + 2;
    let y = area.y + area.height.saturating_sub(h + 1);

    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::ACCENT))
        .title(Span::styled(
            format!(" @file · {} ", q),
            Style::default().fg(theme::DIM),
        ));
    let mut lines: Vec<Line> = Vec::new();
    if files.is_empty() {
        lines.push(Line::from(Span::styled(
            " (no files)",
            Style::default().fg(theme::DIM),
        )));
    } else {
        for (i, file) in files.iter().enumerate() {
            let selected = i == *sel;
            let style = if selected {
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let prefix = if selected { "▸ " } else { "  " };
            lines.push(Line::from(Span::styled(format!("{prefix}{file}"), style)));
        }
    }
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

// ─── Command palette (ctrl+p) ───────────────────────────────────────────

/// set by /update & self-update path; run loop checks after each event
pub static REQUEST_RESTART: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// /exit via execute_command (e.g. through the palette) — the event loop checks
/// the flag because execute_command itself cannot set `quit`.
pub static QUIT_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

const COMMANDS: &[&str] = &[
    "/update",
    "/help",
    "/copy",
    "/context",
    "/memory",
    "/activity",
    "/paste",
    "/select",
    "/clear",
    "/exit",
    "/quit",
    "/resume",
    "/thinking",
    "/metrics",
    "/todos",
    "/attach",
    "/detach",
    "/tools",
    "/runtime",
    "/operations",
    "/history",
    "/brain",
    "/skills",
];

fn canonical_command(command: &str) -> Option<&'static str> {
    match command {
        "/?" => Some("/help"),
        "/q" | "/quit" => Some("/exit"),
        "/knowledge" => Some("/history"),
        other => COMMANDS
            .iter()
            .copied()
            .find(|candidate| *candidate == other),
    }
}

fn clear_current_conversation(app: &mut App) {
    app.entries.clear();
    app.conversation_history.clear();
    app.stream_buf = None;
    app.stream_finalized = false;
    app.bottom_anchor = None;
    app.selected_prompt = -1;
    app.open_tools.clear();
    app.push(
        EntryRole::Status,
        "History cleared. Persisted sessions are unchanged.",
    );
}

fn replace_conversation_with_session(app: &mut App, messages: &[(String, String)]) {
    app.entries.clear();
    app.conversation_history.clear();
    app.stream_buf = None;
    app.stream_finalized = false;
    app.reasoning_buf.clear();
    app.reasoning_idx = None;
    app.open_tools.clear();
    app.selected_prompt = -1;
    app.bottom_anchor = None;
    app.autoscroll = None;
    app.last_stats = None;
    app.session_turns = 0;
    app.session_prompt_tokens = 0;
    app.session_completion_tokens = 0;
    app.live_ctx_chars = 0;
    app.live_ctx_messages = 0;
    for (role, content) in messages {
        let entry_role = match role.as_str() {
            "user" => EntryRole::User,
            "assistant" => EntryRole::Assistant,
            _ => continue,
        };
        app.conversation_history.push(rijo_core::llm::ChatMessage {
            role: role.clone(),
            content: Some(content.clone()),
            ..Default::default()
        });
        app.push(entry_role, content.clone());
    }
}

fn selected_chat_text(entries: &[ChatEntry]) -> String {
    entries
        .iter()
        .map(|entry| entry.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn context_status_text(app: &App) -> String {
    let window = if app.context_window == 0 {
        "unknown".to_string()
    } else {
        fmt_tok(app.context_window.min(u32::MAX as u64) as u32)
    };
    let last_request = app
        .last_stats
        .as_ref()
        .map(|stats| fmt_tok(stats.prompt_tokens));
    let mut status = format!("Context window: {window} tokens");
    if let Some(tokens) = last_request {
        status.push_str(&format!(" · last request: {tokens} provider tokens"));
    }
    if app.running && app.live_ctx_chars > 0 {
        status.push_str(&format!(
            " · current estimate: ~{} tokens / {} message(s)",
            fmt_tok((app.live_ctx_chars / 4).min(u32::MAX as usize) as u32),
            app.live_ctx_messages
        ));
    }
    status.push_str(
        "\nFor stored exchanges, use /history; the context tool also supports recall and compression blocks.",
    );
    status
}

const ACTIVITY_COMPACTION_PREFIX: &str = "Older activity collapsed:";
const MAX_TRANSIENT_ENTRIES: usize = 800;
const MAX_ARCHIVED_ACTIVITY_SNIPPETS: usize = 512;

fn compact_transient_entries(app: &mut App, limit: usize) -> usize {
    let is_transient = |index: usize, entry: &ChatEntry| {
        if entry.text.starts_with(ACTIVITY_COMPACTION_PREFIX) {
            return false;
        }
        match &entry.role {
            EntryRole::Tool => {
                !entry.text.starts_with("◌ ") && !app.open_tools.values().any(|i| *i == index)
            }
            EntryRole::Status | EntryRole::Error => true,
            EntryRole::Thinking => app.reasoning_idx != Some(index),
            EntryRole::User | EntryRole::Assistant | EntryRole::Question | EntryRole::Todo => false,
        }
    };
    let transient_count = app
        .entries
        .iter()
        .enumerate()
        .filter(|(i, entry)| is_transient(*i, entry))
        .count();
    let to_remove = transient_count.saturating_sub(limit);
    if to_remove == 0 {
        return 0;
    }

    let old_entries = std::mem::take(&mut app.entries);
    let mut index_map = vec![None; old_entries.len()];
    let mut kept = Vec::with_capacity(old_entries.len().saturating_sub(to_remove));
    let mut removed = 0usize;
    for (old_index, entry) in old_entries.into_iter().enumerate() {
        if entry.text.starts_with(ACTIVITY_COMPACTION_PREFIX) {
            continue;
        }
        if is_transient(old_index, &entry) && removed < to_remove {
            let role = match &entry.role {
                EntryRole::Tool => "tool",
                EntryRole::Error => "error",
                EntryRole::Thinking => "thinking",
                _ => "status",
            };
            let excerpt: String = entry
                .text
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(160)
                .collect();
            if app.archived_activity.len() == MAX_ARCHIVED_ACTIVITY_SNIPPETS {
                app.archived_activity.remove(0);
            }
            app.archived_activity.push(format!("{role}: {excerpt}"));
            removed += 1;
            continue;
        }
        index_map[old_index] = Some(kept.len());
        kept.push(entry);
    }
    app.compacted_activity_count = app.compacted_activity_count.saturating_add(removed);

    let marker = ChatEntry {
        role: EntryRole::Status,
        text: format!(
            "{ACTIVITY_COMPACTION_PREFIX} {} transient rows; {} recent summaries available via /activity. User/assistant messages remain in the transcript.",
            app.compacted_activity_count,
            app.archived_activity.len()
        ),
        detail: None,
        expanded: false,
    };
    app.entries.reserve(kept.len().saturating_add(1));
    app.entries.push(marker);
    app.entries.extend(kept);

    let old_open_tools = std::mem::take(&mut app.open_tools);
    app.open_tools = old_open_tools
        .into_iter()
        .filter_map(|(id, old_index)| {
            index_map
                .get(old_index)
                .copied()
                .flatten()
                .map(|new_index| (id, new_index + 1))
        })
        .collect();
    app.reasoning_idx = app
        .reasoning_idx
        .and_then(|old_index| index_map.get(old_index).copied().flatten())
        .map(|new_index| new_index + 1);
    app.render_cache.clear();
    app.selection_signature = None;
    removed
}

fn start_tool_entry(app: &mut App, call_id: &str, tool: &str, args_summary: &str) {
    let trimmed = if args_summary.chars().count() > 120 {
        format!("{}…", args_summary.chars().take(117).collect::<String>())
    } else {
        args_summary.to_string()
    };
    app.entries.push(ChatEntry {
        role: EntryRole::Tool,
        text: format!("◌ {tool} {trimmed}"),
        detail: Some(args_summary.to_string()),
        expanded: false,
    });
    app.open_tools
        .insert(call_id.to_string(), app.entries.len() - 1);
}

fn finish_tool_entry(
    app: &mut App,
    call_id: &str,
    tool: &str,
    ok: bool,
    duration_ms: u64,
    excerpt: &str,
    args_json: &str,
) {
    let mark = if ok { "✓" } else { "✗" };
    let duration_str = if duration_ms >= 1000 {
        format!("{:.1}s", duration_ms as f64 / 1000.0)
    } else {
        format!("{duration_ms}ms")
    };
    let excerpt_str = smart_excerpt(tool, args_json, excerpt);
    let keyword = rijo_core::summarize::tool_call(tool, args_json);
    let short = if keyword.is_empty() {
        format!("{mark} {tool} · {duration_str}")
    } else {
        format!("{mark} {tool} · {keyword} · {duration_str}")
    };
    let detail = Some(entry_full_detail(tool, ok, duration_ms, excerpt));

    if let Some(idx) = app
        .open_tools
        .remove(call_id)
        .filter(|idx| *idx < app.entries.len())
    {
        if let Some(entry) = app.entries.get_mut(idx) {
            entry.role = if ok {
                EntryRole::Tool
            } else {
                EntryRole::Error
            };
            entry.text = format!("{short}{excerpt_str}");
            entry.detail = detail;
        }
    } else {
        // Missing or stale call ID: preserve the result as an orphan rather
        // than attaching it to another in-flight call with the same tool name.
        app.entries.push(ChatEntry {
            role: if ok {
                EntryRole::Tool
            } else {
                EntryRole::Error
            },
            text: format!("{short}{excerpt_str}"),
            detail,
            expanded: false,
        });
    }
}

/// A single source for the command overview. Both call sites
/// (input + palette) use it — previously /help listed different
/// commands depending on the path.
fn help_line() -> String {
    let cmds: Vec<&str> = COMMANDS
        .iter()
        .copied()
        .filter(|c| *c != "/thinking" && *c != "/quit")
        .collect();
    format!(
        "Commands: {} · /thinking min|med|max · Keys: ↑/↓ history or scroll · PgUp/PgDn scroll · Tab prompt jump · Ctrl+V paste · Ctrl+O details · Ctrl+K thinking · Ctrl+C quit idle/cancel running",
        cmds.join(" · ")
    )
}

/// Set thinking effort — shared logic for input and palette
/// (the palette passes no argument, then shows the current state).
fn set_thinking_effort(app: &mut App, arg: &str) {
    match arg {
        "min" | "med" | "max" => {
            app.thinking_effort = arg.to_string();
            let label = match arg {
                "min" => "min (low, fastest)",
                "med" => "med (default - efficiency knee)",
                _ => "max (deep thinking, slow)",
            };
            app.push(EntryRole::Status, format!("Thinking effort: {label}"));
        }
        _ => {
            app.push(
                EntryRole::Status,
                format!(
                    "Current thinking effort: {}. Usage: /thinking min|med|max",
                    app.thinking_effort
                ),
            );
        }
    }
}

fn start_history_search(
    app: &mut App,
    state: Arc<rijo_core::AppState>,
    tx: mpsc::UnboundedSender<TuiEvent>,
) {
    let Some(view) = app.history_view.as_mut() else {
        return;
    };
    let kind = view.kind;
    let query = view.query.trim().to_string();
    if query.is_empty() {
        view.error = Some("Type a search phrase first.".to_string());
        return;
    }
    view.searching = true;
    view.error = None;
    view.results.clear();
    view.selected = 0;

    tokio::spawn(async move {
        let (tool, args) = match kind {
            SearchMode::History => (
                "session_search",
                serde_json::json!({"query": query.clone(), "limit": 10}),
            ),
            SearchMode::Brain => (
                "brain_search",
                serde_json::json!({"query": query.clone(), "top_k": 8}),
            ),
        };
        let raw = state.tools.dispatch(tool, args, state.clone()).await;
        let result = match kind {
            SearchMode::History => parse_history_results(&raw),
            SearchMode::Brain => parse_brain_results(&raw),
        };
        let _ = tx.send(TuiEvent::Core(UiEvent::HistoryResults {
            kind,
            query,
            result,
        }));
    });
}

fn request_session_resume(
    app: &mut App,
    executor: Arc<rijo_core::executor::Executor>,
    tx: mpsc::UnboundedSender<TuiEvent>,
    session_id: String,
) {
    if app.running {
        app.push(
            EntryRole::Status,
            "Cannot switch saved sessions while a turn is active.",
        );
        return;
    }
    let (confirm_tx, confirm_rx) = tokio::sync::oneshot::channel();
    app.confirm = Some((Arc::new(confirm_tx), format!(
        "Resume {session_id}? The current in-memory conversation will be replaced; its saved session remains unchanged.",
    )));
    tokio::spawn(async move {
        if !confirm_rx.await.unwrap_or(false) {
            return;
        }
        let state = executor.state();
        let lookup_id = session_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            rijo_core::tools::session_tool::load_session_messages(&state, &lookup_id, 40)
        })
        .await
        .map_err(|e| format!("session load task failed: {e}"))
        .and_then(|result| result);
        let _ = tx.send(TuiEvent::Core(UiEvent::ResumeLoaded { session_id, result }));
    });
}

fn coalesce_adjacent_progress(
    current: &mut rijo_core::executor::ProgressEvent,
    next: rijo_core::executor::ProgressEvent,
) -> Result<(), rijo_core::executor::ProgressEvent> {
    match (&*current, &next) {
        (
            rijo_core::executor::ProgressEvent::ContextEstimate { .. },
            rijo_core::executor::ProgressEvent::ContextEstimate { .. },
        ) => {
            *current = next;
            Ok(())
        }
        (
            rijo_core::executor::ProgressEvent::ToolArgsProgress {
                tool: current_tool, ..
            },
            rijo_core::executor::ProgressEvent::ToolArgsProgress {
                tool: next_tool, ..
            },
        ) if current_tool == next_tool => {
            *current = next;
            Ok(())
        }
        (
            rijo_core::executor::ProgressEvent::Thinking,
            rijo_core::executor::ProgressEvent::Thinking,
        ) => Ok(()),
        _ => match (current, next) {
            (
                rijo_core::executor::ProgressEvent::StreamDelta { text },
                rijo_core::executor::ProgressEvent::StreamDelta { text: next },
            )
            | (
                rijo_core::executor::ProgressEvent::ReasoningDelta { text },
                rijo_core::executor::ProgressEvent::ReasoningDelta { text: next },
            ) => {
                text.push_str(&next);
                Ok(())
            }
            (_, next) => Err(next),
        },
    }
}

async fn forward_progress_events(
    mut progress_rx: mpsc::UnboundedReceiver<rijo_core::executor::ProgressEvent>,
    ui_tx: mpsc::UnboundedSender<TuiEvent>,
) {
    const MAX_COALESCED_EVENTS: usize = 128;
    let mut pending = progress_rx.recv().await;
    while let Some(mut event) = pending {
        pending = None;
        let is_coalescible = matches!(
            &event,
            rijo_core::executor::ProgressEvent::StreamDelta { .. }
                | rijo_core::executor::ProgressEvent::ReasoningDelta { .. }
                | rijo_core::executor::ProgressEvent::ContextEstimate { .. }
                | rijo_core::executor::ProgressEvent::ToolArgsProgress { .. }
                | rijo_core::executor::ProgressEvent::Thinking
        );
        if is_coalescible {
            let mut merged = 0usize;
            loop {
                match progress_rx.try_recv() {
                    Ok(next) => match coalesce_adjacent_progress(&mut event, next) {
                        Ok(()) => {
                            merged += 1;
                            if merged >= MAX_COALESCED_EVENTS {
                                if ui_tx
                                    .send(TuiEvent::Core(UiEvent::Progress(event)))
                                    .is_err()
                                {
                                    return;
                                }
                                tokio::task::yield_now().await;
                                break;
                            }
                            continue;
                        }
                        Err(next) => {
                            if ui_tx
                                .send(TuiEvent::Core(UiEvent::Progress(event)))
                                .is_err()
                            {
                                return;
                            }
                            pending = Some(next);
                            break;
                        }
                    },
                    Err(mpsc::error::TryRecvError::Empty) => {
                        if ui_tx
                            .send(TuiEvent::Core(UiEvent::Progress(event)))
                            .is_err()
                        {
                            return;
                        }
                        break;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        let _ = ui_tx.send(TuiEvent::Core(UiEvent::Progress(event)));
                        return;
                    }
                }
            }
        } else if ui_tx
            .send(TuiEvent::Core(UiEvent::Progress(event)))
            .is_err()
        {
            return;
        }
        if pending.is_none() {
            pending = progress_rx.recv().await;
        }
    }
}

fn fuzzy_commands(query: &str, n: usize) -> Vec<&'static str> {
    if query.is_empty() {
        return COMMANDS.to_vec().into_iter().take(n).collect();
    }
    let mut matcher = nucleo::Matcher::new(nucleo::Config::DEFAULT);
    let pattern = nucleo::pattern::Pattern::parse(
        query,
        nucleo::pattern::CaseMatching::Smart,
        nucleo::pattern::Normalization::Smart,
    );
    let mut candidates: Vec<(u32, &'static str)> = Vec::new();
    let mut scratch = Vec::new();
    for cmd in COMMANDS {
        let haystack = nucleo::Utf32Str::new(cmd, &mut scratch);
        if let Some(score) = pattern.score(haystack, &mut matcher) {
            candidates.push((score, cmd));
        }
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));
    candidates.into_iter().take(n).map(|(_, c)| c).collect()
}

/// Execute a palette command. Returns true if caller should quit.
fn execute_command(app: &mut App, cmd: &str) {
    if app.running && matches!(cmd, "/clear" | "/resume" | "/update") {
        app.push(
            EntryRole::Status,
            format!("Cannot run {cmd} while a turn is active."),
        );
        return;
    }
    match cmd {
        "/help" => {
            app.push(EntryRole::Status, help_line());
        }
        "/tools" => {
            app.tool_explorer = Some((String::new(), 0));
        }
        "/context" => {
            app.push(EntryRole::Status, context_status_text(app));
        }
        "/memory" => {
            if let Some(state) = &app.state {
                app.memory_items = ["memory", "user"]
                    .into_iter()
                    .flat_map(|target| {
                        rijo_core::tools::memory_entries(state, target)
                            .into_iter()
                            .map(|content| MemoryItem {
                                target: target.to_string(),
                                content,
                            })
                    })
                    .collect();
                app.memory_selected = Some(0);
            } else {
                app.push(
                    EntryRole::Status,
                    "Memory view is available only in an interactive session.".to_string(),
                );
            }
        }
        "/activity" => {
            app.activity_view = Some((String::new(), 0));
        }
        "/runtime" => {
            app.runtime_scroll = Some(0);
        }
        "/operations" => {
            app.todos = load_todos();
            app.operations_items = build_operations_items(
                &rijo_core::process::list(),
                &app.todos,
                &load_cron_summaries(),
            );
            app.operations_selected = Some(0);
        }
        "/history" => {
            app.history_view = Some(HistoryView::default());
        }
        "/brain" => {
            app.history_view = Some(HistoryView {
                kind: SearchMode::Brain,
                ..HistoryView::default()
            });
        }
        "/skills" => {
            app.skills_view = Some((String::new(), 0));
        }
        "/attach" => {
            // Reached from the palette, which carries no argument. The inline
            // Enter handler is the one that gets the path.
            app.push(
                EntryRole::Status,
                "Usage: /attach <path>  (image or text file)".to_string(),
            );
        }
        "/detach" => {
            app.attachments.clear();
            app.push(EntryRole::Status, "attachments cleared".to_string());
        }
        "/todos" => {
            // Refresh and show the list in the transcript (the strip always shows
            // the short form).
            app.todos = load_todos();
            if app.todos.is_empty() {
                app.push(EntryRole::Status, "No todos.".to_string());
            } else {
                let text = app
                    .todos
                    .iter()
                    .map(|t| {
                        let (g, _) = todo_glyph(&t.status);
                        format!("{g} {}", t.content)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                app.push(EntryRole::Todo, text);
            }
        }
        "/copy" => {
            let last = app
                .entries
                .iter()
                .rev()
                .find(|e| e.role == EntryRole::Assistant)
                .map(|e| e.text.clone());
            match last {
                Some(text) if !text.is_empty() => {
                    copy_to_clipboard(&text);
                    app.toast = Some((" Copied to clipboard ".to_string(), 25));
                }
                _ => {
                    app.push(EntryRole::Status, "Nothing to copy.".to_string());
                }
            }
        }
        "/paste" => {
            // Palette path (no argument). Ctrl+V and the inline handler do the
            // same thing: an image becomes an attachment, text goes to the input.
            paste_from_clipboard();
        }
        "/select" => {
            // Print the full chat plain text to stdout so the user can
            // terminal-select and copy it.
            let text = selected_chat_text(&app.entries);
            if text.is_empty() {
                app.push(EntryRole::Status, "Chat is empty.".to_string());
                return;
            }
            // Temporarily leave alternate screen to render the text
            // This is the TUI equivalent of "print the chat"
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
            let _ = disable_raw_mode();
            println!("{}", text);
            let _ = enable_raw_mode();
            let _ = execute!(std::io::stdout(), EnterAlternateScreen);
            app.push(EntryRole::Status, "Chat printed — select with the mouse and copy (Ctrl+Shift+C). Press Enter to continue.".to_string());
        }
        "/clear" => {
            clear_current_conversation(app);
        }
        "/update" => match rijo_core::selfupd::pending_update() {
            Some(new_bin) => match rijo_core::selfupd::apply_update(&new_bin) {
                Ok(()) => {
                    let _ = rijo_core::selfupd::write_resume_marker();
                    app.push(
                        EntryRole::Status,
                        "✅ binary replaced — restarting with --resume…".to_string(),
                    );
                    REQUEST_RESTART.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Err(e) => app.push(EntryRole::Error, format!("Update failed: {e}")),
            },
            None => {
                app.push(
                    EntryRole::Status,
                    "No update staged (~/.rijo/staging/rijo.new).".to_string(),
                );
            }
        },
        "/metrics" => {
            let dir = DATA_DIR
                .get()
                .cloned()
                .unwrap_or_else(|| rijo_core::paths::rijo_home().join("data"));
            let s = rijo_core::metrics::summarize(&dir);
            let records = s.get("records").and_then(|r| r.as_u64()).unwrap_or(0);
            if records == 0 {
                app.push(
                    EntryRole::Status,
                    format!("No metrics in {} — run a turn first.", dir.display()),
                );
                return;
            }
            let mut out = vec![format!("Metrics: {records} turns · {}", dir.display())];
            let stat = |v: &serde_json::Value, key: &str| -> Option<String> {
                let e = v.get(key)?;
                Some(format!(
                    "  {key:<12} median {}ms · p95 {}ms · max {}ms",
                    e.get("median").and_then(|x| x.as_u64()).unwrap_or(0),
                    e.get("p95").and_then(|x| x.as_u64()).unwrap_or(0),
                    e.get("max").and_then(|x| x.as_u64()).unwrap_or(0),
                ))
            };
            if let Some(turns) = s.get("turns") {
                for key in ["total_ms", "llm_ms", "tool_ms", "overhead_ms"] {
                    if let Some(l) = stat(turns, key) {
                        out.push(l);
                    }
                }
            }
            if let Some(tok) = s.get("tokens") {
                out.push(format!(
                    "  Tokens       {} prompt / {} completion",
                    tok.get("prompt").and_then(|x| x.as_u64()).unwrap_or(0),
                    tok.get("completion").and_then(|x| x.as_u64()).unwrap_or(0),
                ));
            }
            // prefix caching: the biggest lever against the fixed prompt overhead
            if let Some(c) = s.get("cache") {
                let hit = c.get("hit").and_then(|x| x.as_u64()).unwrap_or(0);
                let miss = c.get("miss").and_then(|x| x.as_u64()).unwrap_or(0);
                if hit + miss > 0 {
                    out.push(format!(
                        "  Cache        {:.0}% hit ({} hit / {} miss)",
                        hit as f64 / (hit + miss) as f64 * 100.0,
                        hit,
                        miss
                    ));
                } else {
                    out.push("  Cache        no provider data".to_string());
                }
            }
            if let Some(fx) = s.get("fixed_prompt_chars") {
                let sysc = fx.get("system").and_then(|x| x.as_u64()).unwrap_or(0);
                let toolsc = fx.get("tools").and_then(|x| x.as_u64()).unwrap_or(0);
                if let Some(pct) = (toolsc * 100).checked_div(sysc + toolsc) {
                    out.push(format!(
                        "  Fixed chars  system prompt {sysc} · tool schemas {toolsc} ({pct}%)"
                    ));
                }
            }
            if let Some(it) = s.get("avg_iterations_per_turn").and_then(|x| x.as_f64()) {
                out.push(format!("  Ø iterations per turn: {it}"));
            }
            if let Some(phases) = s.get("startup_phases").and_then(|p| p.as_object()) {
                let mut lines: Vec<String> = phases
                    .iter()
                    .filter_map(|(k, v)| {
                        stat(&serde_json::json!({ "x": v }), "x").map(|_| {
                            format!(
                                "  {k:<12} median {}ms",
                                v.get("median").and_then(|x| x.as_u64()).unwrap_or(0)
                            )
                        })
                    })
                    .collect();
                if !lines.is_empty() {
                    lines.sort();
                    out.push("startup phases:".to_string());
                    out.extend(lines);
                }
            }
            app.push(EntryRole::Status, out.join("\n"));
        }
        "/thinking" => {
            set_thinking_effort(app, "");
        }
        "/resume" => {
            // main.rs evaluates REQUEST_RESTART and restarts the binary with
            // --resume — that loads the last 20 messages from the DB.
            app.push(
                EntryRole::Status,
                "Session will resume — restarting with --resume…".to_string(),
            );
            REQUEST_RESTART.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        "/exit" | "/quit" | "/q" => {
            // execute_command has no access to `quit` — set the flag,
            // the event loop then shuts down cleanly.
            QUIT_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        _ => {}
    }
}

fn render_palette(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some((q, sel, items)) = &app.palette else {
        return;
    };
    // popup centered, with inner padding and a footer carrying the hint.
    let rows = items.len().max(1) as u16 + 4; // content + footer + border
    let h = rows.min(area.height.saturating_sub(4)).max(5);
    let w = area.width.saturating_sub(8).min(54);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + 2;

    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::rounded())
        .border_style(theme::border_active())
        .title(Span::styled(
            format!(" Commands · {q} "),
            theme::overlay_title(),
        ));
    let inner_w = w.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    if items.is_empty() {
        lines.push(Line::from(Span::styled(" (no match)", theme::dim())));
    } else {
        for (i, cmd) in items.iter().enumerate() {
            let sel_here = i == *sel;
            // selected row filled across the full width — clearer than
            // a bare arrow, and the row looks like a row.
            let text = format!(" {:<pad$}", cmd, pad = inner_w.saturating_sub(1));
            if sel_here {
                lines.push(Line::from(Span::styled(text, theme::selected())));
            } else {
                lines.push(Line::from(Span::styled(text, Style::default())));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑↓ select · Enter run · Esc close",
        theme::dim(),
    )));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn render_tool_explorer(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some((query, selected)) = &app.tool_explorer else {
        return;
    };
    let width = area.width.saturating_sub(4).max(1);
    let height = area.height.saturating_sub(2).max(1);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::rounded())
        .border_style(theme::border_active())
        .title(Span::styled(
            format!(" Tools · {query} "),
            theme::overlay_title(),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let indices = filtered_tool_indices(&app.tool_catalog, query);
    if indices.is_empty() {
        f.render_widget(
            Paragraph::new("No matching tools. Type to search · Esc to close")
                .style(theme::dim())
                .wrap(ratatui::widgets::Wrap { trim: true }),
            inner,
        );
        return;
    }
    let selected = (*selected).min(indices.len().saturating_sub(1));
    let list_height = (inner.height / 3).max(1);
    let sections =
        Layout::vertical([Constraint::Length(list_height), Constraint::Min(1)]).split(inner);
    let items: Vec<_> = indices
        .iter()
        .map(|i| {
            let entry = &app.tool_catalog[*i];
            ratatui::widgets::ListItem::new(Line::from(vec![
                Span::styled(format!("{} · ", entry.toolset), theme::dim()),
                Span::raw(entry.name.as_str()),
            ]))
        })
        .collect();
    let list = ratatui::widgets::List::new(items)
        .highlight_style(theme::selected())
        .highlight_symbol("› ");
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, sections[0], &mut state);

    let entry = &app.tool_catalog[indices[selected]];
    let required = entry
        .schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let actions = entry
        .schema
        .pointer("/properties/action/enum")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .unwrap_or_default();
    let mut detail = format!("{} · {}\n{}", entry.name, entry.toolset, entry.description);
    if !required.is_empty() {
        detail.push_str(&format!("\nRequired: {required}"));
    }
    if !actions.is_empty() {
        detail.push_str(&format!("\nActions: {actions}"));
    }
    detail.push_str("\nEnter inserts a prompt · ↑/↓ select · Esc close");
    f.render_widget(
        Paragraph::new(detail).wrap(ratatui::widgets::Wrap { trim: true }),
        sections[1],
    );
}

fn render_skills_panel(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some((query, selected)) = &app.skills_view else {
        return;
    };
    let width = area.width.saturating_sub(4).clamp(1, 78);
    let height = area.height.saturating_sub(2).max(1);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::rounded())
        .border_style(theme::border_active())
        .title(Span::styled(
            format!(" Skills · {query} "),
            theme::overlay_title(),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let indices = filtered_skill_indices(&app.skills_catalog, query);
    if indices.is_empty() {
        f.render_widget(
            Paragraph::new("No matching skills. Search the configured safe skill roots.")
                .style(theme::dim())
                .wrap(ratatui::widgets::Wrap { trim: true }),
            inner,
        );
        return;
    }

    let selected = (*selected).min(indices.len().saturating_sub(1));
    let list_height = (inner.height / 3).max(1);
    let sections =
        Layout::vertical([Constraint::Length(list_height), Constraint::Min(1)]).split(inner);
    let items: Vec<_> = indices
        .iter()
        .map(|i| {
            let skill = &app.skills_catalog[*i];
            let scope = if skill.own { "local" } else { "library" };
            ratatui::widgets::ListItem::new(format!(
                "{} · {} · {scope}",
                skill.category, skill.name
            ))
        })
        .collect();
    let list = ratatui::widgets::List::new(items)
        .highlight_style(theme::selected())
        .highlight_symbol("› ");
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, sections[0], &mut state);

    let skill = &app.skills_catalog[indices[selected]];
    let detail = format!(
        "{} · {} · {}\n{}\n\nEnter inserts a prompt to load this skill via the registered skill tools.",
        skill.name,
        skill.category,
        if skill.own { "local" } else { "library" },
        skill.description
    );
    f.render_widget(
        Paragraph::new(detail).wrap(ratatui::widgets::Wrap { trim: true }),
        sections[1],
    );
}

fn render_memory_panel(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some(selected) = app.memory_selected else {
        return;
    };
    let width = area.width.saturating_sub(6).clamp(1, 78);
    let height = area.height.saturating_sub(2).max(1);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::rounded())
        .border_style(theme::border_active())
        .title(Span::styled(" Memory · read only ", theme::overlay_title()));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    if app.memory_items.is_empty() {
        f.render_widget(
            Paragraph::new("No saved memory entries in MEMORY.md or USER.md.")
                .style(theme::dim())
                .wrap(ratatui::widgets::Wrap { trim: true }),
            inner,
        );
        return;
    }
    let selected = selected.min(app.memory_items.len() - 1);
    let list_height = (inner.height / 3).max(1);
    let sections =
        Layout::vertical([Constraint::Length(list_height), Constraint::Min(1)]).split(inner);
    let items: Vec<_> = app
        .memory_items
        .iter()
        .map(|item| {
            let preview = item.content.lines().next().unwrap_or("");
            ratatui::widgets::ListItem::new(format!(
                "{} · {}",
                item.target,
                preview.chars().take(60).collect::<String>()
            ))
        })
        .collect();
    let list = ratatui::widgets::List::new(items)
        .highlight_style(theme::selected())
        .highlight_symbol("› ");
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, sections[0], &mut state);

    let item = &app.memory_items[selected];
    f.render_widget(
        Paragraph::new(format!(
            "{} memory entry\n{}\n\nEnter inserts this entry into the composer as context; it does not edit the saved store.",
            item.target, item.content
        ))
        .wrap(ratatui::widgets::Wrap { trim: true }),
        sections[1],
    );
}

fn render_activity_archive_panel(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some((query, selected)) = &app.activity_view else {
        return;
    };
    let width = area.width.saturating_sub(6).clamp(1, 82);
    let height = area.height.saturating_sub(2).max(1);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::rounded())
        .border_style(theme::border_active())
        .title(Span::styled(
            format!(" Archived activity · {query} "),
            theme::overlay_title(),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let indices = filtered_activity_indices(&app.archived_activity, query);
    if indices.is_empty() {
        let text = if app.compacted_activity_count == 0 {
            "No activity has been compacted yet."
        } else {
            "No recent summary matches. Raw transient details were discarded during compaction; user/assistant messages remain available in /history."
        };
        f.render_widget(
            Paragraph::new(text).wrap(ratatui::widgets::Wrap { trim: true }),
            inner,
        );
        return;
    }

    let selected = (*selected).min(indices.len().saturating_sub(1));
    let list_height = (inner.height / 3).max(1);
    let sections =
        Layout::vertical([Constraint::Length(list_height), Constraint::Min(1)]).split(inner);
    let items: Vec<_> = indices
        .iter()
        .map(|i| ratatui::widgets::ListItem::new(app.archived_activity[*i].as_str()))
        .collect();
    let list = ratatui::widgets::List::new(items)
        .highlight_style(theme::selected())
        .highlight_symbol("› ");
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, sections[0], &mut state);

    let snippet = &app.archived_activity[indices[selected]];
    let detail = format!(
        "{snippet}\n\nSearch covers the most recent {} compact summaries. Original transient details were not persisted.",
        app.archived_activity.len()
    );
    f.render_widget(
        Paragraph::new(detail).wrap(ratatui::widgets::Wrap { trim: true }),
        sections[1],
    );
}

fn render_runtime_panel(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some(scroll) = app.runtime_scroll else {
        return;
    };
    let width = area.width.saturating_sub(6).clamp(1, 78);
    let height = area.height.saturating_sub(2).max(1);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::rounded())
        .border_style(theme::border_active())
        .title(Span::styled(
            " Runtime · capabilities ",
            theme::overlay_title(),
        ));
    f.render_widget(
        Paragraph::new(app.runtime_lines.join("\n"))
            .block(block)
            .scroll((scroll, 0))
            .wrap(ratatui::widgets::Wrap { trim: true }),
        rect,
    );
}

fn render_operations_panel(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some(selected) = app.operations_selected else {
        return;
    };
    let width = area.width.saturating_sub(6).clamp(1, 82);
    let height = area.height.saturating_sub(2).max(1);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::rounded())
        .border_style(theme::border_active())
        .title(Span::styled(
            " Operations · status ",
            theme::overlay_title(),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    if app.operations_items.is_empty() {
        f.render_widget(
            Paragraph::new(
                "No background processes, todos, or scheduled jobs. Enter a task to create work.",
            )
            .wrap(ratatui::widgets::Wrap { trim: true }),
            inner,
        );
        return;
    }

    let selected = selected.min(app.operations_items.len() - 1);
    let list_height = (inner.height / 3).max(1);
    let sections =
        Layout::vertical([Constraint::Length(list_height), Constraint::Min(1)]).split(inner);
    let items: Vec<_> = app
        .operations_items
        .iter()
        .map(|item| ratatui::widgets::ListItem::new(item.title.as_str()))
        .collect();
    let list = ratatui::widgets::List::new(items)
        .highlight_style(theme::selected())
        .highlight_symbol("› ");
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, sections[0], &mut state);

    let item = &app.operations_items[selected];
    let detail = format!(
        "{}\n{}\n\nEnter inserts a prompt into the composer. Operations still run through the agent tools and their existing guards.\n{}",
        item.title, item.detail, item.prompt
    );
    f.render_widget(
        Paragraph::new(detail).wrap(ratatui::widgets::Wrap { trim: true }),
        sections[1],
    );
}

fn render_history_panel(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some(view) = app.history_view.as_ref() else {
        return;
    };
    let width = area.width.saturating_sub(6).clamp(1, 82);
    let height = area.height.saturating_sub(2).max(1);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::rounded())
        .border_style(theme::border_active())
        .title(Span::styled(
            format!(
                " {} · {} ",
                match view.kind {
                    SearchMode::History => "History",
                    SearchMode::Brain => "Brain",
                },
                view.query
            ),
            theme::overlay_title(),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    if view.results.is_empty() {
        let message = if let Some(error) = &view.error {
            error.as_str()
        } else if view.searching {
            match view.kind {
                SearchMode::History => "Searching local session history…",
                SearchMode::Brain => "Searching the indexed workspace…",
            }
        } else if view.query.trim().is_empty() {
            match view.kind {
                SearchMode::History => "Type a phrase and press Enter to search saved sessions.",
                SearchMode::Brain => {
                    "Type a query and press Enter to search indexed workspace code."
                }
            }
        } else {
            match view.kind {
                SearchMode::History => {
                    "No matches yet. Press Enter to search, or keep typing to refine."
                }
                SearchMode::Brain => {
                    "No matches yet. Press Enter to search, or keep typing to refine."
                }
            }
        };
        f.render_widget(
            Paragraph::new(match view.kind {
                SearchMode::History => format!(
                    "{message}\n\nSearch uses the local session index. Enter on a result inserts that exchange as context; it does not resume or overwrite the current session."
                ),
                SearchMode::Brain => format!(
                    "{message}\n\nSearch uses the workspace brain and may fall back to full text. Results are pointers and bounded excerpts; no model/API call is made."
                ),
            })
            .wrap(ratatui::widgets::Wrap { trim: true }),
            inner,
        );
        return;
    }

    let selected = view.selected.min(view.results.len().saturating_sub(1));
    let list_height = (inner.height / 3).max(1);
    let sections =
        Layout::vertical([Constraint::Length(list_height), Constraint::Min(1)]).split(inner);
    let items: Vec<_> = view
        .results
        .iter()
        .map(|hit| {
            ratatui::widgets::ListItem::new(format!(
                "{} · {} · {}",
                hit.session_id, hit.role, hit.timestamp
            ))
        })
        .collect();
    let list = ratatui::widgets::List::new(items)
        .highlight_style(theme::selected())
        .highlight_symbol("› ");
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, sections[0], &mut state);

    let hit = &view.results[selected];
    let detail = match view.kind {
        SearchMode::History => format!(
            "{} · {} · {}\n{}\n\nEnter inserts this bounded excerpt as context. Ctrl+R resumes this saved session after confirmation.",
            hit.session_id, hit.role, hit.timestamp, hit.excerpt
        ),
        SearchMode::Brain => format!(
            "{} · {} · {}\n{}\n\nEnter inserts this bounded code excerpt as context; it does not open or modify the source file.",
            hit.session_id, hit.role, hit.timestamp, hit.excerpt
        ),
    };
    f.render_widget(
        Paragraph::new(detail).wrap(ratatui::widgets::Wrap { trim: true }),
        sections[1],
    );
}

/// Confirmation dialog overlay (centered, blocks all input until y/n).
fn render_confirm(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some((_, message)) = &app.confirm else {
        return;
    };
    let w = area.width.saturating_sub(10).min(64);
    let text_w = w.saturating_sub(4) as usize;
    // wrap the message so it is not cut off (previously a
    // long path ended in the middle of a word).
    let wrapped = wrap_for_dialog(message, text_w);
    // border (2) + blank line (1) + button row (1)
    let h = (wrapped.len() as u16 + 4).min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + area.height.saturating_sub(h + 1);

    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(theme::rounded())
        .border_style(Style::default().fg(theme::ERR))
        .title(Span::styled(
            " Confirm ",
            Style::default().fg(theme::ERR).add_modifier(Modifier::BOLD),
        ));
    let mut lines: Vec<Line> = Vec::new();
    for l in &wrapped {
        lines.push(Line::from(Span::raw(format!("  {l}"))));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "  (Y) ",
            Style::default().fg(theme::OK).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Confirm", Style::default().fg(theme::OK)),
        Span::styled("      (N/Esc) ", theme::dim()),
        Span::styled("Cancel", theme::dim()),
    ]));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// Simple word wrap for dialog texts (width in characters).
fn wrap_for_dialog(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.is_empty() {
            line.push_str(word);
        } else if line.chars().count() + 1 + word.chars().count() <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(std::mem::take(&mut line));
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod display_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn ordinary_q_and_c_are_inserted_into_the_prompt() {
        let mut app = App::new();

        handle_character(&mut app, 'q');
        handle_character(&mut app, 'c');

        assert_eq!(app.input, "qc");
        assert_eq!(app.cursor, 2);
        assert!(app.toast.is_none());
    }

    #[test]
    fn thinking_toggle_shortcut_requires_ctrl_so_plain_k_is_typeable() {
        let plain = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
        let ctrl = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert!(!thinking_toggle_requested(&plain, true));
        assert!(thinking_toggle_requested(&ctrl, true));
        assert!(!thinking_toggle_requested(&ctrl, false));
        assert!(allows_text_entry(KeyModifiers::NONE));
        assert!(allows_text_entry(KeyModifiers::SHIFT));
        assert!(!allows_text_entry(KeyModifiers::CONTROL));
    }

    #[test]
    fn idle_timer_ticks_do_not_request_redraws() {
        let idle = App::new();
        assert!(!tick_requires_redraw(&idle, false, false));
        assert_eq!(tick_interval(&idle), std::time::Duration::from_millis(750));

        let mut running = App::new();
        running.running = true;
        assert!(tick_requires_redraw(&running, false, false));
        assert_eq!(
            tick_interval(&running),
            std::time::Duration::from_millis(80)
        );

        let mut dragging = App::new();
        dragging.autoscroll = Some(true);
        assert!(tick_requires_redraw(&dragging, false, false));
        assert_eq!(
            tick_interval(&dragging),
            std::time::Duration::from_millis(30)
        );
        assert!(tick_requires_redraw(&idle, true, false));
        assert!(tick_requires_redraw(&idle, false, true));
    }

    #[test]
    fn entry_render_cache_key_tracks_same_length_text_and_layout_changes() {
        let base = ChatEntry {
            role: EntryRole::Assistant,
            text: "alpha".into(),
            detail: Some("raw args".into()),
            expanded: false,
        };
        let mut changed = base.clone();
        changed.text = "bravo".into();
        assert_ne!(
            entry_render_signature(&base, 2, 80, false),
            entry_render_signature(&changed, 2, 80, false)
        );
        assert_ne!(
            entry_render_signature(&base, 2, 80, false),
            entry_render_signature(&base, 2, 60, false)
        );
        assert_ne!(
            entry_render_signature(&base, 2, 80, false),
            entry_render_signature(&base, 2, 80, true)
        );
    }

    #[test]
    fn viewport_collects_only_lines_inside_the_visible_scroll_window() {
        let cache = vec![Some(EntryRenderCache {
            signature: 1,
            lines: ["a", "b", "c"].map(Line::from).to_vec(),
        })];
        let stream = [Line::from("d"), Line::from("e")];

        let visible = collect_visible_chat_lines(&cache, &stream, 2, 3);
        let text: Vec<String> = visible
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(text, vec!["c", "d", "e"]);
    }

    #[test]
    fn long_transcripts_reuse_unchanged_entry_render_cache() {
        let mut app = App::new();
        app.entries = (0..1200)
            .map(|i| ChatEntry {
                role: if i % 2 == 0 {
                    EntryRole::User
                } else {
                    EntryRole::Assistant
                },
                text: format!("entry {i}: repeated text for the viewport cache"),
                detail: None,
                expanded: false,
            })
            .collect();

        let _ = render_frame_text(&mut app, 80, 24);
        let original: Vec<u64> = app
            .render_cache
            .iter()
            .map(|entry| entry.as_ref().unwrap().signature)
            .collect();
        let _ = render_frame_text(&mut app, 80, 24);
        let repeated: Vec<u64> = app
            .render_cache
            .iter()
            .map(|entry| entry.as_ref().unwrap().signature)
            .collect();
        assert_eq!(
            original, repeated,
            "unchanged entries retain their cached layout"
        );

        app.entries[0].text = "ENTRY 0: repeated text for the viewport cache".into();
        let _ = render_frame_text(&mut app, 80, 24);
        assert_ne!(
            app.render_cache[0].as_ref().unwrap().signature,
            original[0],
            "same-length text changes invalidate that entry"
        );
        assert_eq!(
            app.render_cache[1].as_ref().unwrap().signature,
            original[1],
            "unmodified later entries retain their cached layout"
        );
    }

    #[test]
    fn transient_compaction_preserves_messages_and_remaps_live_tool_indices() {
        let mut app = App::new();
        app.push(EntryRole::User, "keep user message");
        app.push(EntryRole::Tool, "✓ read_file · old.rs · 3ms");
        app.push(EntryRole::Tool, "✓ patch · old.rs · 1ms");
        app.push(EntryRole::Status, "old status note");
        app.push(EntryRole::Error, "✗ old tool error");
        app.push(EntryRole::Assistant, "keep assistant message");
        start_tool_entry(
            &mut app,
            "active-call",
            "read_file",
            r#"{"path":"live.rs"}"#,
        );

        let removed = compact_transient_entries(&mut app, 2);

        assert_eq!(removed, 2);
        assert!(app.entries.iter().any(|e| e.text == "keep user message"));
        assert!(app
            .entries
            .iter()
            .any(|e| e.text == "keep assistant message"));
        assert!(app.entries[0].text.starts_with("Older activity collapsed:"));
        assert_eq!(app.compacted_activity_count, 2);
        assert_eq!(app.archived_activity.len(), 2);
        let live_idx = app.open_tools["active-call"];
        assert!(app.entries[live_idx].text.starts_with("◌ read_file"));
        assert!(app.entries.iter().any(|e| e.text == "✗ old tool error"));

        execute_command(&mut app, "/activity");
        if let Some((query, _)) = app.activity_view.as_mut() {
            query.push_str("old.rs");
        }
        let frame = render_frame_text(&mut app, 100, 24);
        assert!(frame.contains("Archived activity · old.rs"), "{frame}");
        assert!(frame.contains("read_file"), "{frame}");
    }

    #[tokio::test]
    async fn progress_forwarder_coalesces_ephemeral_events_without_reordering() {
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        let (ui_tx, mut tui_rx) = mpsc::unbounded_channel();
        progress_tx
            .send(rijo_core::executor::ProgressEvent::StreamDelta { text: "A".into() })
            .unwrap();
        progress_tx
            .send(rijo_core::executor::ProgressEvent::StreamDelta { text: "B".into() })
            .unwrap();
        progress_tx
            .send(rijo_core::executor::ProgressEvent::Thinking)
            .unwrap();
        progress_tx
            .send(rijo_core::executor::ProgressEvent::ReasoningDelta { text: "R1".into() })
            .unwrap();
        progress_tx
            .send(rijo_core::executor::ProgressEvent::ReasoningDelta { text: "R2".into() })
            .unwrap();
        progress_tx
            .send(rijo_core::executor::ProgressEvent::ContextEstimate {
                chars: 100,
                messages: 2,
            })
            .unwrap();
        progress_tx
            .send(rijo_core::executor::ProgressEvent::ContextEstimate {
                chars: 200,
                messages: 3,
            })
            .unwrap();
        progress_tx
            .send(rijo_core::executor::ProgressEvent::ToolArgsProgress {
                tool: "read_file".into(),
                total_chars: 5,
                preview: "src/".into(),
            })
            .unwrap();
        progress_tx
            .send(rijo_core::executor::ProgressEvent::ToolArgsProgress {
                tool: "read_file".into(),
                total_chars: 18,
                preview: "src/lib.rs".into(),
            })
            .unwrap();
        drop(progress_tx);

        forward_progress_events(progress_rx, ui_tx).await;

        let events: Vec<_> = std::iter::from_fn(|| tui_rx.try_recv().ok()).collect();
        assert_eq!(events.len(), 5);
        assert!(matches!(
            &events[0],
            TuiEvent::Core(UiEvent::Progress(rijo_core::executor::ProgressEvent::StreamDelta { text })) if text == "AB"
        ));
        assert!(matches!(
            events[1],
            TuiEvent::Core(UiEvent::Progress(
                rijo_core::executor::ProgressEvent::Thinking
            ))
        ));
        assert!(matches!(
            &events[2],
            TuiEvent::Core(UiEvent::Progress(rijo_core::executor::ProgressEvent::ReasoningDelta { text })) if text == "R1R2"
        ));
        assert!(matches!(
            events[3],
            TuiEvent::Core(UiEvent::Progress(
                rijo_core::executor::ProgressEvent::ContextEstimate {
                    chars: 200,
                    messages: 3
                }
            ))
        ));
        assert!(matches!(
            &events[4],
            TuiEvent::Core(UiEvent::Progress(rijo_core::executor::ProgressEvent::ToolArgsProgress {
                tool,
                preview,
                total_chars: 18
            })) if tool == "read_file" && preview == "src/lib.rs"
        ));
    }

    #[test]
    fn ctrl_c_quits_when_idle_and_cancels_when_running() {
        assert_eq!(ctrl_c_action(false), CtrlCAction::Quit);
        assert_eq!(ctrl_c_action(true), CtrlCAction::Cancel);
    }

    #[test]
    fn typed_aliases_resolve_to_the_palette_command() {
        assert_eq!(canonical_command("/?"), Some("/help"));
        assert_eq!(canonical_command("/q"), Some("/exit"));
        assert_eq!(canonical_command("/quit"), Some("/exit"));
        assert_eq!(canonical_command("/knowledge"), Some("/history"));
        for cmd in COMMANDS {
            if *cmd == "/quit" {
                continue;
            }
            assert_eq!(canonical_command(cmd), Some(*cmd), "{cmd}");
        }
        assert_eq!(canonical_command("/not-a-command"), None);
    }

    #[test]
    fn clear_command_resets_visible_and_in_memory_conversation_state() {
        let mut app = App::new();
        app.entries.push(ChatEntry {
            role: EntryRole::Assistant,
            text: "old answer".into(),
            detail: None,
            expanded: false,
        });
        app.conversation_history.push(rijo_core::llm::ChatMessage {
            role: "user".into(),
            content: Some("old prompt".into()),
            ..Default::default()
        });

        execute_command(&mut app, "/clear");

        assert!(app.conversation_history.is_empty());
        assert_eq!(app.entries.len(), 1);
        assert_eq!(
            app.entries[0].text,
            "History cleared. Persisted sessions are unchanged."
        );
        assert!(!selected_chat_text(&app.entries).contains("old answer"));
    }

    #[test]
    fn palette_cannot_clear_active_conversation_during_a_running_turn() {
        let mut app = App::new();
        app.running = true;
        app.entries.push(ChatEntry {
            role: EntryRole::Assistant,
            text: "active answer".into(),
            detail: None,
            expanded: false,
        });
        app.conversation_history.push(rijo_core::llm::ChatMessage {
            role: "user".into(),
            content: Some("active prompt".into()),
            ..Default::default()
        });

        execute_command(&mut app, "/clear");

        assert!(app
            .entries
            .iter()
            .any(|entry| entry.text == "active answer"));
        assert_eq!(app.conversation_history.len(), 1);
        assert!(app.entries.iter().any(|entry| {
            entry
                .text
                .contains("Cannot run /clear while a turn is active")
        }));
    }

    #[test]
    fn parallel_calls_with_the_same_tool_complete_by_call_id() {
        let mut app = App::new();
        start_tool_entry(&mut app, "call-a", "read_file", r#"{"path":"alpha.txt"}"#);
        start_tool_entry(&mut app, "call-b", "read_file", r#"{"path":"beta.txt"}"#);

        finish_tool_entry(
            &mut app,
            "call-b",
            "read_file",
            true,
            12,
            "BETA_RESULT",
            r#"{"path":"beta.txt"}"#,
        );
        finish_tool_entry(
            &mut app,
            "call-a",
            "read_file",
            true,
            20,
            "ALPHA_RESULT",
            r#"{"path":"alpha.txt"}"#,
        );

        assert!(app.entries[0].text.contains("alpha.txt"));
        assert!(app.entries[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("ALPHA_RESULT"));
        assert!(app.entries[1].text.contains("beta.txt"));
        assert!(app.entries[1]
            .detail
            .as_deref()
            .unwrap()
            .contains("BETA_RESULT"));
        assert!(app.open_tools.is_empty());
    }

    #[test]
    fn select_text_includes_tool_results_mutated_without_a_new_entry_event() {
        let mut app = App::new();
        start_tool_entry(&mut app, "call", "read_file", r#"{"path":"notes.md"}"#);
        finish_tool_entry(
            &mut app,
            "call",
            "read_file",
            true,
            4,
            "fresh result",
            r#"{"path":"notes.md"}"#,
        );

        let selected = selected_chat_text(&app.entries);
        assert!(selected.contains("fresh result"), "{selected:?}");
        assert!(
            !selected.contains("◌ read_file"),
            "stale progress: {selected:?}"
        );
    }

    #[test]
    fn tool_catalog_contains_every_registered_tool_and_its_actions() {
        let registry = rijo_core::tools::Registry::build_default();
        let catalog = build_tool_catalog(&registry);
        let names = registry.get_names();

        assert_eq!(catalog.len(), names.len());
        for name in names {
            assert_eq!(catalog.iter().filter(|entry| entry.name == name).count(), 1);
        }
        let github = catalog.iter().find(|entry| entry.name == "github").unwrap();
        assert!(!github.description.is_empty());
        assert!(github.schema["properties"]["action"]["enum"].is_array());
        assert_eq!(github.toolset, "github");
        let matches = filtered_tool_indices(&catalog, "browser");
        assert!(!matches.is_empty());
        assert!(matches
            .iter()
            .all(|i| { catalog[*i].name.contains("browser") || catalog[*i].toolset == "browser" }));
        assert!(palette_command_needs_argument("/thinking"));
        assert!(palette_command_needs_argument("/attach"));
        assert!(!palette_command_needs_argument("/tools"));

        let mut app = App::new();
        app.input = "draft".into();
        insert_palette_command_prefix(&mut app, "/thinking");
        assert_eq!(app.input, "/thinking draft");
        assert_eq!(app.cursor, "/thinking ".len());
    }

    #[test]
    fn skills_directory_searches_registered_safe_skill_entries() {
        let skills = vec![
            rijo_core::tools::skills_tool::SkillEntry {
                name: "rust-harness".into(),
                description: "Rust workspace testing conventions".into(),
                category: "development".into(),
                own: true,
            },
            rijo_core::tools::skills_tool::SkillEntry {
                name: "paper-review".into(),
                description: "Review scientific papers".into(),
                category: "research".into(),
                own: false,
            },
        ];

        let matches = filtered_skill_indices(&skills, "rust");

        assert_eq!(matches, vec![0]);
        let mut app = App::new();
        app.skills_catalog = skills;
        execute_command(&mut app, "/skills");
        if let Some((query, _)) = app.skills_view.as_mut() {
            query.push_str("rust");
        }
        let frame = render_frame_text(&mut app, 100, 24);
        assert!(frame.contains("Skills · rust"), "{frame}");
        assert!(frame.contains("rust-harness"), "{frame}");
        assert!(frame.contains("load this skill"), "{frame}");
    }

    #[test]
    fn context_command_reports_measurements_without_mutating_history() {
        let mut app = App::new();
        app.context_window = 128_000;
        app.last_stats = Some(TurnStatsView {
            iterations: 1,
            llm_ms: 900,
            tool_ms: 0,
            total_ms: 900,
            tool_calls: 0,
            prompt_tokens: 4_402,
            last_prompt_tokens: 4_402,
            completion_tokens: 5,
            cache_hit: 0,
            cache_miss: 0,
            failures: 0,
            brain_hits: 0,
            ttft_ms: 0,
            context_window: 128_000,
        });
        app.conversation_history.push(rijo_core::llm::ChatMessage {
            role: "user".into(),
            content: Some("keep me".into()),
            ..Default::default()
        });

        execute_command(&mut app, "/context");

        let status = app.entries.last().unwrap().text.as_str();
        assert!(status.contains("128.0k"), "{status}");
        assert!(status.contains("4.4k"), "{status}");
        assert_eq!(app.conversation_history.len(), 1);
    }

    #[test]
    fn resuming_a_selected_session_replaces_only_the_in_memory_context() {
        let mut app = App::new();
        app.push(EntryRole::Assistant, "current draft history");
        app.conversation_history.push(rijo_core::llm::ChatMessage {
            role: "assistant".into(),
            content: Some("current draft history".into()),
            ..Default::default()
        });
        app.session_turns = 4;

        replace_conversation_with_session(
            &mut app,
            &[
                ("user".into(), "old question".into()),
                ("assistant".into(), "old answer".into()),
            ],
        );

        assert_eq!(app.conversation_history.len(), 2);
        assert_eq!(
            app.conversation_history[0].content.as_deref(),
            Some("old question")
        );
        assert_eq!(app.entries.len(), 2);
        assert_eq!(app.entries[0].text, "old question");
        assert_eq!(app.session_turns, 0);
        assert_eq!(app.selected_prompt, -1);
    }

    #[test]
    fn memory_view_reads_user_owned_entries_and_never_edits_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = rijo_core::config::Config::default();
        config.data_dir = dir.path().to_path_buf();
        let memory_dir = config.data_dir.join("memories");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join("MEMORY.md"), "prefers concise patches").unwrap();
        let state = Arc::new(rijo_core::AppState {
            brain: Arc::new(
                rijo_core::brain::db::BrainDb::open(&config.data_dir.join("brain.db"), 384)
                    .unwrap(),
            ),
            embedder: tokio::sync::Mutex::new(None),
            config,
            session_id: rijo_core::AppState::new_session_id(),
            tools: Arc::new(rijo_core::tools::Registry::build_default()),
            ui_tx: std::sync::Mutex::new(None),
            files_read: std::sync::Mutex::new(std::collections::HashSet::new()),
            cancel: std::sync::atomic::AtomicBool::new(false),
            router: std::sync::Mutex::new(None),
        });
        let mut app = App::new();
        app.state = Some(state);

        execute_command(&mut app, "/memory");

        assert_eq!(app.memory_items.len(), 1);
        assert_eq!(app.memory_items[0].target, "memory");
        let frame = render_frame_text(&mut app, 100, 24);
        assert!(frame.contains("Memory · read only"), "{frame}");
        assert!(frame.contains("prefers concise patches"), "{frame}");
        assert!(frame.contains("does not edit"), "{frame}");
        assert_eq!(
            std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap(),
            "prefers concise patches"
        );
    }

    #[test]
    fn tools_command_renders_searchable_registry_metadata() {
        let registry = rijo_core::tools::Registry::build_default();
        let mut app = App::new();
        app.tool_catalog = build_tool_catalog(&registry);
        execute_command(&mut app, "/tools");
        if let Some((query, _)) = app.tool_explorer.as_mut() {
            query.push_str("github");
        }

        let frame = render_frame_text(&mut app, 100, 24);
        assert!(frame.contains("Tools · github"), "{frame}");
        assert!(frame.contains("github · github"), "{frame}");
        assert!(
            frame.contains("repo_view"),
            "action enum is visible:\n{frame}"
        );
        assert!(
            frame.contains("Required:"),
            "schema fields are visible:\n{frame}"
        );
    }

    #[test]
    fn optional_box_tools_also_get_human_summaries_and_targets() {
        let summary = smart_excerpt("box_status", "{}", r#"{"status":"ready"}"#);
        assert!(summary.contains("ready"), "{summary}");
        assert_eq!(
            rijo_core::summarize::tool_call("box_fs_read", r#"{"path":"src/lib.rs"}"#),
            "lib.rs"
        );
    }

    #[test]
    fn tool_directory_renders_without_panicking_at_narrow_sizes() {
        let registry = rijo_core::tools::Registry::build_default();
        for (width, height) in [(40, 12), (60, 18), (80, 24), (120, 36)] {
            let mut app = App::new();
            app.tool_catalog = build_tool_catalog(&registry);
            app.tool_explorer = Some(("github".into(), 0));
            let frame = render_frame_text(&mut app, width, height);
            assert!(
                frame.contains("Tools"),
                "missing overlay at {width}x{height}: {frame}"
            );
            assert!(
                frame.contains("github"),
                "missing selected tool at {width}x{height}: {frame}"
            );
        }
    }

    #[test]
    fn every_overlay_renders_across_the_terminal_size_matrix() {
        let registry = rijo_core::tools::Registry::build_default();
        let check = |mut app: App, width, height, expected: &str| {
            let frame = render_frame_text(&mut app, width, height);
            assert!(
                frame.contains(expected),
                "{expected:?} missing at {width}x{height}:\n{frame}"
            );
        };
        for (width, height) in [(40, 12), (60, 18), (80, 24), (100, 30), (160, 48)] {
            let mut tools = App::new();
            tools.tool_catalog = build_tool_catalog(&registry);
            tools.tool_explorer = Some(("github".into(), 0));
            check(tools, width, height, "Tools · github");

            let mut skills = App::new();
            skills.skills_catalog = vec![rijo_core::tools::skills_tool::SkillEntry {
                name: "rust-harness".into(),
                description: "Workspace tests".into(),
                category: "dev".into(),
                own: true,
            }];
            skills.skills_view = Some(("rust".into(), 0));
            check(skills, width, height, "Skills · rust");

            let mut memory = App::new();
            memory.memory_items = vec![MemoryItem {
                target: "user".into(),
                content: "Prefers concise answers".into(),
            }];
            memory.memory_selected = Some(0);
            check(memory, width, height, "Memory · read only");

            let mut runtime = App::new();
            runtime.runtime_lines =
                build_runtime_lines(&rijo_core::config::Config::default(), 31, false, false);
            runtime.runtime_scroll = Some(0);
            check(runtime, width, height, "Runtime · capabilities");

            let mut operations = App::new();
            operations.operations_items = vec![OperationsItem {
                title: "proc_test · Running".into(),
                detail: "PID 5".into(),
                prompt: "Poll the process".into(),
            }];
            operations.operations_selected = Some(0);
            check(operations, width, height, "Operations · status");

            let mut history = App::new();
            history.history_view = Some(HistoryView {
                query: "server".into(),
                results: vec![HistoryHit {
                    session_id: "sess_test".into(),
                    role: "user".into(),
                    timestamp: "2026-10-16".into(),
                    excerpt: "start the server".into(),
                }],
                ..HistoryView::default()
            });
            check(history, width, height, "History · server");

            let mut brain = App::new();
            brain.history_view = Some(HistoryView {
                kind: SearchMode::Brain,
                query: "router".into(),
                results: vec![HistoryHit {
                    session_id: "src/router.rs".into(),
                    role: "code".into(),
                    timestamp: "score 0.875".into(),
                    excerpt: "safe tool subset".into(),
                }],
                ..HistoryView::default()
            });
            check(brain, width, height, "Brain · router");

            let mut archive = App::new();
            archive.archived_activity = vec!["tool: read_file · src/lib.rs".into()];
            archive.compacted_activity_count = 20;
            archive.activity_view = Some(("src/lib".into(), 0));
            check(archive, width, height, "Archived activity · src/lib");

            let mut palette = App::new();
            palette.palette = Some(("/tools".into(), 0, fuzzy_commands("/tools", 10)));
            check(palette, width, height, "Commands · /tools");

            let mut confirm = App::new();
            let (confirm_tx, _confirm_rx) = tokio::sync::oneshot::channel::<bool>();
            confirm.confirm = Some((Arc::new(confirm_tx), "Confirm this action".into()));
            check(confirm, width, height, "Confirm");

            let mut ask = App::new();
            let (ask_tx, _ask_rx) = tokio::sync::oneshot::channel::<Vec<(String, String)>>();
            ask.ask = Some(AskState::new(
                vec![AskQuestion {
                    id: "q".into(),
                    question: "Which command?".into(),
                    options: vec![AskOption {
                        label: "cargo test".into(),
                        recommended: true,
                    }],
                    allow_free_text: true,
                }],
                Arc::new(ask_tx),
            ));
            check(ask, width, height, "question");
        }
    }

    #[test]
    fn runtime_capability_summary_never_renders_credential_values() {
        let mut config = rijo_core::config::Config::default();
        config.llm.api_key = "llm-secret-fixture".into();
        config.router.enabled = true;
        let dir = tempfile::tempdir().unwrap();
        config.data_dir = dir.path().to_path_buf();
        let memory_dir = config.data_dir.join("memories");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join("MEMORY.md"), "private-memory-fixture").unwrap();
        let mut env = std::collections::BTreeMap::new();
        env.insert("MCP_TOKEN".into(), "mcp-secret-fixture".into());
        config.mcp.servers.push(rijo_core::config::McpServerConfig {
            name: "docs".into(),
            command: "docs-mcp".into(),
            args: Vec::new(),
            env,
            read_only_tools: vec!["search".into()],
            enabled: true,
        });

        let lines = build_runtime_lines(&config, 31, true, true);
        let joined = lines.join("\n");
        assert!(joined.contains("key configured"), "{joined}");
        assert!(joined.contains("docs"), "{joined}");
        assert!(joined.contains("Router: loaded"), "{joined}");
        assert!(
            joined.contains("Memory:") && joined.contains("bytes stored"),
            "{joined}"
        );
        assert!(!joined.contains("llm-secret-fixture"), "{joined}");
        assert!(!joined.contains("mcp-secret-fixture"), "{joined}");
        assert!(!joined.contains("private-memory-fixture"), "{joined}");

        let mut app = App::new();
        app.runtime_lines = lines;
        app.runtime_scroll = Some(0);
        let frame = render_frame_text(&mut app, 100, 24);
        assert!(frame.contains("Runtime"), "{frame}");
        assert!(frame.contains("LLM model:"), "{frame}");
        assert!(!frame.contains("llm-secret-fixture"), "{frame}");
        assert!(!frame.contains("mcp-secret-fixture"), "{frame}");
        assert!(!frame.contains("private-memory-fixture"), "{frame}");
    }

    #[test]
    fn operations_view_uses_stable_process_ids_and_surfaces_todos() {
        let processes = vec![rijo_core::process::Info {
            id: "proc_0000002a".into(),
            pid: 42,
            command: "cargo run --release".into(),
            workdir: "C:/work".into(),
            running: true,
            exit_code: None,
            uptime_ms: 12_000,
            stdout_path: String::new(),
            stderr_path: String::new(),
        }];
        let todos = vec![TodoView {
            content: "Patch the API client".into(),
            status: "in_progress".into(),
        }];

        let items = build_operations_items(&processes, &todos, &[("job-7".into(), "1h".into())]);

        assert_eq!(items.len(), 3);
        assert!(items[0].title.contains("proc_0000002a"));
        assert!(items[0].title.contains("Running"));
        assert!(items[0].prompt.contains("proc_0000002a"));
        assert!(items[1].title.contains("Patch the API client"));
        assert!(items[2].title.contains("job-7"));
        assert!(!items[2].prompt.contains("remove"));

        let mut app = App::new();
        app.operations_items = items;
        app.operations_selected = Some(0);
        let frame = render_frame_text(&mut app, 100, 24);
        assert!(frame.contains("Operations"), "{frame}");
        assert!(frame.contains("Poll background process"), "{frame}");
        assert!(frame.contains("existing guards"), "{frame}");
    }

    #[test]
    fn choosing_an_operation_preserves_an_existing_draft() {
        let mut app = App::new();
        app.input = "after that, explain the logs".into();
        app.cursor = app.input.len();

        insert_operation_prompt(&mut app, "Poll background process proc_0000002a");

        assert_eq!(
            app.input,
            "Poll background process proc_0000002a\nafter that, explain the logs"
        );
        assert_eq!(app.cursor, "Poll background process proc_0000002a\n".len());
        assert!(app.operations_selected.is_none());
    }

    #[test]
    fn history_search_parses_bounded_provenance_results() {
        let hits = parse_history_results(
            r#"{"results":[{"session_id":"sess_20261015","role":"assistant","ts":"2026-10-15T12:00:00Z","content":"Fixed the router and added tests."}]}"#,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "sess_20261015");
        assert!(hits[0].excerpt.contains("Fixed the router"));
        assert!(parse_history_results(r#"{"error":"FTS query failed"}"#).is_err());
    }

    #[test]
    fn brain_search_results_keep_source_kind_and_score() {
        let hits = parse_brain_results(
            r#"{"mode":"hybrid","hits":[{"source":"src/router.rs","kind":"code","score":0.875,"excerpt":"Router chooses tools"}]}"#,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "src/router.rs");
        assert_eq!(hits[0].role, "code");
        assert_eq!(hits[0].timestamp, "score 0.875");
        assert!(hits[0].excerpt.contains("Router chooses"));
        let prompt = search_hit_prompt(SearchMode::Brain, &hits[0]);
        assert!(prompt.contains("src/router.rs"), "{prompt}");
        assert!(prompt.contains("indexed workspace result"), "{prompt}");
    }

    #[test]
    fn history_panel_shows_provenance_and_safe_insert_semantics() {
        let mut app = App::new();
        app.history_view = Some(HistoryView {
            kind: SearchMode::History,
            query: "router".into(),
            selected: 0,
            results: vec![HistoryHit {
                session_id: "sess_20261015".into(),
                role: "assistant".into(),
                timestamp: "2026-10-15T12:00:00Z".into(),
                excerpt: "The router uses a local bundle.".into(),
            }],
            searching: false,
            error: None,
        });

        let frame = render_frame_text(&mut app, 100, 24);

        assert!(frame.contains("History · router"), "{frame}");
        assert!(frame.contains("sess_20261015"), "{frame}");
        assert!(frame.contains("local bundle"), "{frame}");
        assert!(frame.contains("Ctrl+R resumes"), "{frame}");
    }

    #[tokio::test]
    async fn history_overlay_searches_through_the_registered_session_tool() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = rijo_core::config::Config::default();
        config.data_dir = dir.path().to_path_buf();
        let workspace = config
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| config.workspace.clone())
            .to_string_lossy()
            .to_string();
        let brain = Arc::new(
            rijo_core::brain::db::BrainDb::open(&config.data_dir.join("brain.db"), 384).unwrap(),
        );
        brain
            .upsert_node(
                &rijo_core::brain::db::Node {
                    id: "router-node".into(),
                    label: "Router selection".into(),
                    content: "The calibrated router chooses a safe tool subset.".into(),
                    kind: "code".into(),
                    source: "src/router.rs".into(),
                    workspace,
                    token_cost: 12,
                    created_at: 1,
                    updated_at: 1,
                    access_cnt: 0,
                    relevance: 1.0,
                },
                None,
            )
            .unwrap();
        let state = Arc::new(rijo_core::AppState {
            brain,
            embedder: tokio::sync::Mutex::new(None),
            config,
            session_id: rijo_core::AppState::new_session_id(),
            tools: Arc::new(rijo_core::tools::Registry::build_default()),
            ui_tx: std::sync::Mutex::new(None),
            files_read: std::sync::Mutex::new(std::collections::HashSet::new()),
            cancel: std::sync::atomic::AtomicBool::new(false),
            router: std::sync::Mutex::new(None),
        });
        let session_id = rijo_core::tools::session_tool::current_session_id();
        rijo_core::tools::session_tool::record_message(
            &state,
            &session_id,
            "assistant",
            "The local router uses a calibrated bundle.",
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.history_view = Some(HistoryView {
            query: "router".into(),
            ..HistoryView::default()
        });

        start_history_search(&mut app, state.clone(), tx.clone());

        let event = rx.recv().await.expect("search result event");
        match event {
            TuiEvent::Core(UiEvent::HistoryResults {
                kind,
                query,
                result,
            }) => {
                assert_eq!(kind, SearchMode::History);
                assert_eq!(query, "router");
                let hits = result.expect("session search succeeds");
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].session_id, session_id);
                assert!(hits[0].excerpt.contains("calibrated bundle"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        app.history_view = Some(HistoryView {
            kind: SearchMode::Brain,
            query: "router".into(),
            ..HistoryView::default()
        });
        start_history_search(&mut app, state, tx);
        let event = rx.recv().await.expect("brain search result event");
        match event {
            TuiEvent::Core(UiEvent::HistoryResults {
                kind,
                query,
                result,
            }) => {
                assert_eq!(kind, SearchMode::Brain);
                assert_eq!(query, "router");
                let hits = result.expect("brain search succeeds");
                assert!(hits.iter().any(|hit| hit.session_id == "src/router.rs"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn selected_session_resume_waits_for_confirmation_then_loads() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = rijo_core::config::Config::default();
        config.data_dir = dir.path().to_path_buf();
        let executor = Arc::new(rijo_core::executor::Executor::new_opts(config, false));
        let state = executor.state();
        rijo_core::tools::session_tool::record_message(
            &state,
            "sess_archive-7",
            "user",
            "old task",
        );
        rijo_core::tools::session_tool::record_message(
            &state,
            "sess_archive-7",
            "assistant",
            "old answer",
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();

        request_session_resume(&mut app, executor, tx, "sess_archive-7".into());

        let (confirmation, _) = app.confirm.take().expect("confirmation dialog");
        Arc::try_unwrap(confirmation)
            .expect("only the TUI owns the sender")
            .send(true)
            .expect("resume approval delivered");
        match rx.recv().await.expect("resume result event") {
            TuiEvent::Core(UiEvent::ResumeLoaded { session_id, result }) => {
                assert_eq!(session_id, "sess_archive-7");
                let messages = result.expect("session load succeeds");
                assert_eq!(messages.len(), 2);
                assert_eq!(messages[0].1, "old task");
                assert_eq!(messages[1].1, "old answer");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Look-and-feel assertions: border shape, input prompt, dialog wrapping.
    /// That makes "looks better" checkable instead of merely claimed.
    #[test]
    fn look_input_and_dialog() {
        let mut app = snapshot_app(128_000);
        let frame = render_frame_text(&mut app, 100, 22);
        assert!(
            frame.contains("╭─"),
            "input box needs a rounded border:\n{frame}"
        );
        assert!(
            frame.contains("❯ thanks"),
            "input needs a prompt:\\n{frame}"
        );
        // The heavy full-width rule between entries is gone (only the
        // fine line at the role header remains).
        assert!(
            !frame
                .lines()
                .any(|l| l.trim_end().chars().all(|c| c == '─') && l.trim().len() > 20),
            "no continuous separator line expected anymore:\n{frame}"
        );

        // confirm: long text is wrapped, not cut off.
        let mut app = snapshot_app(128_000);
        let (tx, _rx) = tokio::sync::oneshot::channel::<bool>();
        app.confirm = Some((
            Arc::new(tx),
            "DELETE src/main.rs? This file will be deleted permanently.".into(),
        ));
        let frame = render_frame_text(&mut app, 100, 22);
        assert!(
            frame.contains("permanently."),
            "message must be complete:\n{frame}"
        );
        assert!(frame.contains("(Y)"), "buttons must be visible:\n{frame}");
    }

    /// The idle input needs no "Input" title — the `❯` prompt already says it.
    /// The running state does, because the input is locked then.
    #[test]
    fn input_title_only_in_the_running_state() {
        let mut app = snapshot_app(128_000);
        let idle = render_frame_text(&mut app, 100, 22);
        assert!(
            !idle.contains("╭ Input"),
            "the idle input repeats itself:\n{idle}"
        );
        assert!(idle.contains("❯"), "the prompt marks the input:\n{idle}");

        let mut app = snapshot_app(128_000);
        app.running = true;
        let running = render_frame_text(&mut app, 100, 22);
        assert!(
            running.contains("running…"),
            "a locked input must say so:\n{running}"
        );
    }

    /// The old code did `rest.split_at(1)` — a BYTE index. With the cursor at 0
    /// and an "ü" first that panicked ("byte index 1 is not a char boundary"):
    /// pressing Home before an umlaut crashed the TUI.
    #[test]
    fn input_handles_a_multibyte_char_at_the_cursor() {
        let mut app = snapshot_app(128_000);
        app.input = "über die Grenze".into();
        app.cursor = 0;
        let frame = render_frame_text(&mut app, 60, 22);
        assert!(frame.contains("über"), "input must render:\n{frame}");
    }

    /// Long input scrolls horizontally so the cursor stays visible; before this,
    /// the text simply ran into the border and the cursor was gone.
    #[test]
    fn input_scrolls_to_keep_the_cursor_visible() {
        let mut app = snapshot_app(128_000);
        app.input = format!("HEAD{}TAIL", "x".repeat(200));
        app.cursor = app.input.len();
        let frame = render_frame_text(&mut app, 40, 22);
        let rows: Vec<&str> = frame.lines().collect();
        let input_row = rows
            .get(rows.len().saturating_sub(2))
            .copied()
            .unwrap_or("");
        assert!(
            input_row.contains("TAIL"),
            "the tail must be visible:\n{frame}"
        );
        assert!(
            !input_row.contains("HEAD"),
            "the start must have scrolled off:\n{frame}"
        );
    }

    /// The arrows scroll the chat when there is nothing to edit; with text (or an
    /// active recall) they walk the input history instead. Regression: the mouse
    /// wheel is reported as ↑/↓ by some Windows terminals, and Ctrl+↑/↓ was
    /// shadowed by the unguarded arrow arm.
    #[test]
    fn arrows_scroll_only_with_an_empty_input() {
        let mut app = snapshot_app(128_000);
        app.input.clear();
        app.history_idx = None;
        assert!(up_down_scrolls(&app), "empty input: ↑/↓ scroll the chat");

        app.input = "read Cargo.toml".into();
        assert!(!up_down_scrolls(&app), "typing: ↑ recalls history");

        app.input.clear();
        app.history_idx = Some(1);
        assert!(
            !up_down_scrolls(&app),
            "a recall in progress stays on history"
        );
    }

    /// The answer is the chosen option, or the typed free text when the
    /// "Anderer …" row is selected. Options-less questions are free text only.
    #[test]
    fn the_answer_assembles_options_and_free_text() {
        let (tx, _rx) = tokio::sync::oneshot::channel::<Vec<(String, String)>>();
        let mut ask = AskState::new(
            vec![
                AskQuestion {
                    id: "a".into(),
                    question: "A?".into(),
                    options: vec![
                        AskOption {
                            label: "one".into(),
                            recommended: true,
                        },
                        AskOption {
                            label: "two".into(),
                            recommended: false,
                        },
                    ],
                    allow_free_text: true,
                },
                AskQuestion {
                    id: "b".into(),
                    question: "B?".into(),
                    options: vec![],
                    allow_free_text: false,
                },
            ],
            Arc::new(tx),
        );
        ask.choice[0] = 1;
        assert_eq!(ask.answer(0), "two", "an option answers directly");
        assert_eq!(
            ask.free_text_row(0),
            2,
            "the free-text row follows the options"
        );
        ask.choice[0] = 2;
        ask.text[0] = "  eigener Wert ".into();
        assert_eq!(ask.answer(0), "eigener Wert", "free text, trimmed");
        // No options at all: the only row is the free-text one.
        assert_eq!(ask.rows(1).len(), 1);
        assert_eq!(ask.free_text_row(1), 0);
        ask.text[1] = "frei".into();
        assert_eq!(
            ask.answers(),
            vec![
                ("a".to_string(), "eigener Wert".to_string()),
                ("b".to_string(), "frei".to_string()),
            ]
        );
    }

    /// The todo strip alone.
    fn render_todos_text(app: &App, width: u16) -> String {
        let height = todo_strip_height(app).max(1);
        let mut term = Terminal::new(TestBackend::new(width, height)).expect("term");
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        term.draw(|f| render_todos(f, area, app))
            .expect("draw todos");
        buffer_text(term.backend().buffer(), width, height)
    }

    /// Attached files are listed above the input, so it is clear WHAT will be
    /// sent before Enter is pressed.
    #[test]
    fn the_attachment_strip_lists_the_files() {
        let mut app = App::new();
        assert!(app.attachments.is_empty(), "nothing attached by default");
        app.attachments = vec!["/tmp/photo.png".into(), "/tmp/notes.md".into()];
        let frame = render_frame_text(&mut app, 80, 22);
        assert!(frame.contains("attached ×2"), "{frame}");
        assert!(frame.contains("photo.png"), "{frame}");
        assert!(frame.contains("notes.md"), "{frame}");
        assert!(frame.contains("/detach"), "{frame}");
    }

    /// A copied image becomes an attachment — that is the whole reason the
    /// clipboard is read at all.
    #[test]
    fn pasting_an_image_attaches_it() {
        let mut app = App::new();
        paste_read(
            &mut app,
            crate::clipboard::ClipboardRead::Image("C:/tmp/shot.png".into()),
        );
        assert_eq!(app.attachments, vec!["C:/tmp/shot.png".to_string()]);
        assert!(
            app.entries
                .iter()
                .any(|e| e.text.contains("attached from clipboard")),
            "the user has to see that it worked"
        );
        assert!(
            app.input.is_empty(),
            "an image must not type itself into the input"
        );
    }

    /// Text goes to the input and must NOT become an attachment.
    #[test]
    fn pasting_text_goes_into_the_input() {
        let mut app = App::new();
        paste_read(
            &mut app,
            crate::clipboard::ClipboardRead::Text("hello\nworld".into()),
        );
        assert_eq!(app.input, "hello\nworld");
        assert!(app.attachments.is_empty());
    }

    /// While the palette is open the text belongs to the palette, not to the
    /// input underneath — the same routing the bracketed-paste event uses.
    #[test]
    fn pasting_text_reaches_the_open_palette() {
        let mut app = App::new();
        app.palette = Some((String::new(), 0, fuzzy_commands("", 30)));
        paste_read(
            &mut app,
            crate::clipboard::ClipboardRead::Text("/co".into()),
        );
        let (query, _, _) = app.palette.as_ref().expect("palette");
        assert_eq!(query, "/co");
        assert!(app.input.is_empty());
    }

    /// A copied or dragged-in image FILE arrives as a path; it must become an
    /// attachment, not a path typed into the prompt.
    #[test]
    fn pasting_an_image_path_attaches_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("dropped.png");
        std::fs::write(&p, b"\x89PNG\r\n").unwrap();
        let mut app = App::new();
        paste_read(
            &mut app,
            crate::clipboard::ClipboardRead::Text(p.display().to_string()),
        );
        assert_eq!(app.attachments.len(), 1, "the file has to be attached");
        assert!(
            app.input.is_empty(),
            "a path must not be typed into the prompt: {}",
            app.input
        );
    }

    /// ...but a path that is NOT an image stays text: the user may be talking
    /// about a file, not pasting a picture.
    #[test]
    fn pasting_a_non_image_path_stays_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("notes.md");
        std::fs::write(&p, "hi").unwrap();
        let mut app = App::new();
        paste_read(
            &mut app,
            crate::clipboard::ClipboardRead::Text(p.display().to_string()),
        );
        assert!(app.attachments.is_empty());
        assert!(app.input.contains("notes.md"), "{}", app.input);
    }

    #[test]
    fn pasting_an_empty_clipboard_only_toasts() {
        let mut app = App::new();
        paste_read(&mut app, crate::clipboard::ClipboardRead::Empty);
        assert!(app.toast.is_some(), "silence would look like a broken key");
        assert!(app.attachments.is_empty());
        assert!(app.input.is_empty());
    }

    /// The strip is reserved only when there is a list; collapsed it shows
    /// progress and the current task, expanded one line per todo.
    #[test]
    fn the_todo_strip_shows_progress_and_the_current_task() {
        let mut app = App::new();
        assert_eq!(todo_strip_height(&app), 0, "no list -> no rows reserved");

        app.todos = vec![
            TodoView {
                content: "Recherche".into(),
                status: "completed".into(),
            },
            TodoView {
                content: "Fehler isolieren".into(),
                status: "in_progress".into(),
            },
            TodoView {
                content: "Patch".into(),
                status: "pending".into(),
            },
        ];
        assert_eq!(todo_strip_height(&app), 1, "collapsed = one line");
        let text = render_todos_text(&app, 80);
        assert!(text.contains("1/3"), "{text}");
        assert!(text.contains("Fehler isolieren"), "{text}");
        assert!(
            !text.contains("open"),
            "the open count is already in done/total: {text}"
        );
        assert!(
            text.contains('▰') || text.contains('▱'),
            "a progress bar next to the count: {text}"
        );
        assert!(
            text.trim_end().ends_with("Ctrl+T"),
            "hint at the edge: {text}"
        );

        app.todos_expanded = true;
        assert_eq!(todo_strip_height(&app), 3, "expanded = one line per todo");
        let text = render_todos_text(&app, 80);
        assert!(text.contains("✔ Recherche"), "{text}");
        assert!(text.contains("▸ Fehler isolieren"), "{text}");
        assert!(text.contains("☐ Patch"), "{text}");
    }

    /// A long item is ellipsised and the hint keeps its place at the right edge.
    #[test]
    fn a_long_todo_is_ellipsised_and_keeps_the_hint_at_the_edge() {
        let mut app = App::new();
        app.todos = vec![
            TodoView {
                content: "done".into(),
                status: "completed".into(),
            },
            TodoView {
                content: "x".repeat(120),
                status: "in_progress".into(),
            },
        ];
        let text = render_todos_text(&app, 60);
        assert!(text.contains('…'), "long item ellipsised: {text:?}");
        assert!(text.contains("1/2"), "{text:?}");
        assert!(text.contains("Ctrl+T"), "{text:?}");
        assert!(
            text.trim_end().chars().count() <= 60,
            "never overflows: {}",
            text.trim_end().chars().count()
        );
    }

    /// A pasted line break must not send the prompt. The rule: only an Enter that
    /// arrives within a few milliseconds of the previous key is part of a paste.
    #[test]
    fn only_a_tight_burst_counts_as_a_paste() {
        assert!(!is_paste_burst(None), "the first event is never a burst");
        assert!(is_paste_burst(Some(std::time::Duration::from_millis(0))));
        assert!(is_paste_burst(Some(std::time::Duration::from_millis(2))));
        assert!(
            !is_paste_burst(Some(std::time::Duration::from_millis(80))),
            "a human keypress is not a paste"
        );
    }

    /// The send rule, explicitly: an unmodified Enter sends only when nothing
    /// follows it and nothing preceded it in a burst. This is what prevents a
    /// pasted prompt from being split.
    #[test]
    fn an_enter_sends_only_when_nothing_else_is_pending() {
        assert!(
            !enter_is_pasted_newline(false, false),
            "an isolated Enter sends"
        );
        assert!(
            enter_is_pasted_newline(true, false),
            "an Enter right after other keys is a pasted line break"
        );
        assert!(
            enter_is_pasted_newline(false, true),
            "an Enter with more input already waiting is a pasted line break"
        );
    }

    /// A paste is inserted as ONE edit (not one redraw per character) and keeps    /// its line structure.
    #[test]
    fn a_paste_is_one_edit_and_keeps_its_lines() {
        let mut app = App::new();
        app.input = "ab".into();
        app.cursor = 1;
        apply_paste(&mut app, "X\r\nY\tz\r\n");
        // \r\n -> \n, tab -> space; the line breaks survive.
        assert_eq!(app.input, "aX\nY z\nb");
        assert_eq!(app.cursor, "aX\nY z\n".len());
    }

    /// The paste must not be able to smuggle an escape sequence into the buffer.
    #[test]
    fn a_paste_drops_control_characters() {
        let mut app = App::new();
        apply_paste(&mut app, "ok\u{1b}[2Jevil");
        assert_eq!(app.input, "ok[2Jevil", "the ESC must be dropped");
    }

    #[test]
    fn the_input_wraps_per_character_and_tracks_the_cursor() {
        // width 4: "abcdef" -> "abcd" / "ef"
        let (rows, r, c) = wrap_input("abcdef", 6, 4);
        assert_eq!(rows, vec!["abcd".to_string(), "ef".to_string()]);
        assert_eq!((r, c), (1, 2), "cursor at the end of the second row");
        // A cursor exactly on the wrap boundary belongs to the next row.
        let (_, r, c) = wrap_input("abcdef", 4, 4);
        assert_eq!((r, c), (1, 0));
        // Explicit line breaks are rows of their own.
        let (rows, r, c) = wrap_input("a\nb\n", 4, 10);
        assert_eq!(rows, vec!["a".to_string(), "b".to_string(), String::new()]);
        assert_eq!((r, c), (2, 0), "cursor on the trailing empty line");
    }

    #[test]
    fn the_arrows_move_within_a_multi_line_prompt() {
        let mut app = App::new();
        app.input = "first\nsecond\nthird".into();
        app.cursor = app.input.len();
        let line_of = |app: &App| {
            let s = &app.input[..app.cursor];
            let start = s.rfind('\n').map(|i| i + 1).unwrap_or(0);
            let end = app.input[app.cursor..]
                .find('\n')
                .map(|i| app.cursor + i)
                .unwrap_or(app.input.len());
            app.input[start..end].to_string()
        };
        cursor_move_line(&mut app, -1);
        assert_eq!(line_of(&app), "second", "↑ moves to the line above");
        cursor_move_line(&mut app, 1);
        assert_eq!(app.cursor, app.input.len(), "↓ returns to the end");
        assert_eq!(line_of(&app), "third");
    }

    /// The selected row of the command palette is filled (not just bold) —
    /// the color is checked directly from the buffer, not from the text.
    #[test]
    fn palette_selection_is_filled() {
        let mut term = Terminal::new(TestBackend::new(100, 22)).expect("term");
        let mut app = snapshot_app(128_000);
        app.palette = Some(("/".into(), 1, fuzzy_commands("/", 10)));
        term.draw(|f| draw_frame(f, &mut app)).expect("draw");
        let buf = term.backend().buffer();
        let filled = (0..22u16).any(|y| {
            (0..100u16).any(|x| {
                buf.cell((x, y))
                    .map(|c| c.style().bg == Some(theme::ACCENT))
                    .unwrap_or(false)
            })
        });
        assert!(filled, "selected row must be filled with a color");
    }

    /// Status line: nothing is cut in the middle of a value. Previously it ended
    /// with "Cache 26% (1.0k/4.0" on a narrow terminal — that looked like an error.
    #[test]
    fn status_line_never_cuts_a_value() {
        let mut app = snapshot_app(128_000);
        let narrow = render_frame_text(&mut app, 60, 22);
        let rows: Vec<&str> = narrow.lines().collect();
        // Bottom-up: 3 rows input box, then the data row, then the activity row.
        let data_row = rows
            .get(rows.len().saturating_sub(4))
            .copied()
            .unwrap_or("");
        let activity_row = rows
            .get(rows.len().saturating_sub(5))
            .copied()
            .unwrap_or("");
        assert!(
            !data_row.contains("(1.0k/"),
            "no truncated number pair: {data_row:?}"
        );
        assert!(
            !data_row.trim_end().ends_with("…k") && !data_row.trim_end().ends_with("k/"),
            "no half number in the data row: {data_row:?}"
        );
        assert!(
            !activity_row.ends_with("=De…"),
            "short hints are better omitted than trimmed: {activity_row:?}"
        );
    }

    /// A long prompt grows the input box instead of scrolling off one line.
    #[test]
    fn the_input_box_grows_with_the_prompt() {
        let mut app = snapshot_app(128_000);
        app.input = "x".repeat(300);
        app.cursor = app.input.len();
        let frame = render_frame_text(&mut app, 60, 24);
        let content_rows = frame.lines().filter(|l| l.starts_with('│')).count();
        assert!(
            content_rows >= 2,
            "the box did not grow past one content row:\n{frame}"
        );
        // Continuation rows carry the indent, not the prompt.
        assert!(
            frame.lines().any(|l| l.starts_with("│   ")),
            "no continuation row found:\n{frame}"
        );
    }

    /// Every entry carries a two-column origin marker, and the markers differ per
    /// role — that is the "where does this line come from" signal. A tool result's
    /// own ✓/✗ is promoted into the column instead of being doubled.
    #[test]
    fn every_role_shows_its_own_origin_marker() {
        let mut app = App::new();
        for (role, text) in [
            (EntryRole::User, "hi"),
            (EntryRole::Assistant, "ok"),
            (EntryRole::Thinking, "hmm"),
            (EntryRole::Tool, "✓ terminal · 3ms"),
            (EntryRole::Error, "✗ read_file · 3ms"),
            (EntryRole::Question, "Welcher Befehl?"),
            (EntryRole::Todo, "1/2 · isolieren"),
            (EntryRole::Status, "hinweis"),
        ] {
            app.entries.push(ChatEntry {
                role,
                text: text.to_string(),
                detail: None,
                expanded: false,
            });
        }
        let frame = render_frame_text(&mut app, 100, 40);
        for marker in [
            "● you",
            "◆ rijo",
            "⟡ thinking",
            "✓ terminal",
            "✗ read_file",
            "? question",
            "✔ todos",
            "· hinweis",
        ] {
            assert!(
                frame.contains(marker),
                "missing origin marker {marker:?}:\n{frame}"
            );
        }
    }

    /// Separators are consistent and never dangle: every value after the first is
    /// introduced by " · ", and a shortened line ends on " …", not " · …".
    #[test]
    fn status_rows_never_dangle_a_separator() {
        let mut app = App::new();
        app.session_turns = 2;
        app.session_prompt_tokens = 40_000;
        app.session_completion_tokens = 500;
        app.last_stats = Some(TurnStatsView {
            iterations: 3,
            llm_ms: 1100,
            tool_ms: 200,
            total_ms: 1400,
            tool_calls: 5,
            prompt_tokens: 44_400,
            last_prompt_tokens: 44_400,
            completion_tokens: 33,
            cache_hit: 3200,
            cache_miss: 800,
            failures: 1,
            brain_hits: 4,
            ttft_ms: 620,
            context_window: 128_000,
        });
        for width in [50u16, 60, 80, 100, 120, 200] {
            for row in [
                render_activity_text(&app, width),
                render_data_text(&app, width),
            ] {
                let t = row.trim_end();
                assert!(!t.ends_with(" ·"), "dangling separator at {width}: {row:?}");
                assert!(
                    !t.ends_with("· …"),
                    "dangling separator before the cut at {width}: {row:?}"
                );
                assert!(!t.contains("  · "), "double separator at {width}: {row:?}");
            }
        }
    }

    /// The drop order is explicit: on a narrow row the hint goes before the state,
    /// and a value that does not fit is dropped whole (never cut in the middle).
    #[test]
    fn hints_are_dropped_before_the_state_on_a_narrow_row() {
        let mut app = App::new();
        app.running = true;
        app.phase = "✎ composing the final summary".into();
        app.turn_started = Some(std::time::Instant::now());

        // Wide enough for state + hint: the hint sits at the right edge.
        let wide = render_activity_text(&app, 80);
        assert!(wide.contains("composing"), "{wide:?}");
        assert!(wide.contains("Ctrl+C aborts"), "{wide:?}");
        assert!(
            wide.trim_end().ends_with("aborts"),
            "the hint is right-aligned: {wide:?}"
        );

        // Too narrow for both: the hint goes, the state stays.
        let narrow = render_activity_text(&app, 40);
        assert!(narrow.contains("composing"), "state survives: {narrow:?}");
        assert!(!narrow.contains("Ctrl+C"), "hint dropped first: {narrow:?}");
    }

    /// The tool-args preview names the target, not a char count.
    #[test]
    fn args_target_finds_the_human_target_in_partial_json() {
        assert_eq!(
            rijo_core::llm::args_target(r#"{"path":"crates/rijo-core/src/tui.rs""#).as_deref(),
            Some("crates/rijo-core/src/tui.rs")
        );
        assert_eq!(
            rijo_core::llm::args_target(r#"{"command":"cargo test""#).as_deref(),
            Some("cargo test")
        );
        // An empty value and an unknown key are "no target", not an error.
        assert_eq!(
            rijo_core::llm::args_target(r#"{"path":"""#).as_deref(),
            None
        );
        assert_eq!(
            rijo_core::llm::args_target(r#"{"other":1"#).as_deref(),
            None
        );
        assert!(rijo_core::llm::args_target("").is_none());
        // A long path keeps its useful tail.
        let long = format!(r#"{{"path":"{}"#, "a/".repeat(40));
        let t = rijo_core::llm::args_target(&long).unwrap();
        assert!(t.starts_with('…') && t.chars().count() <= 39, "{t}");
    }

    /// Idle the measurement joins the state row and the data row collapses to zero
    /// height; while a turn runs both rows stay (GOAL-100 §V.3).
    #[test]
    fn idle_merges_the_measurement_into_the_state_row() {
        let mut app = snapshot_app(128_000);
        assert_eq!(data_row_height(&app), 0, "idle with stats -> one row");

        let activity = render_activity_text(&app, 120);
        assert!(activity.contains("ready"), "{activity}");
        assert!(
            activity.contains("context "),
            "the measurement joins the state row: {activity}"
        );

        app.running = true;
        assert_eq!(data_row_height(&app), 1, "running -> two rows");

        let fresh = App::new();
        assert_eq!(data_row_height(&fresh), 1, "no stats -> keep the hint row");
    }

    /// The running animation is one cell wide and actually moves.
    #[test]
    fn the_spinner_is_single_cell_and_moves() {
        let frames: Vec<&str> = (0..12).map(spinner_glyph).collect();
        for f in &frames {
            assert_eq!(f.chars().count(), 1, "one cell: {f:?}");
        }
        assert!(
            frames[0] != frames[1] && frames[1] != frames[2],
            "consecutive frames differ: {frames:?}"
        );
        assert_eq!(spinner_glyph(10), spinner_glyph(0), "wraps without a panic");
    }

    /// The strip shows where Rijo Code is right now: the in-progress item with a
    /// live spinner while running, the next open one (labelled) when none is marked.
    #[test]
    fn the_strip_shows_where_we_are_right_now() {
        let mut app = App::new();
        app.todos = vec![
            TodoView {
                content: "Recherche".into(),
                status: "completed".into(),
            },
            TodoView {
                content: "Patch".into(),
                status: "pending".into(),
            },
        ];
        let idle = render_todos_text(&app, 80);
        assert!(
            idle.contains("next: Patch"),
            "no in_progress -> show the next open one: {idle:?}"
        );

        app.todos[1].status = "in_progress".into();
        app.running = true;
        app.spinner_frame = 3;
        let running = render_todos_text(&app, 80);
        assert!(running.contains("Patch"), "{running:?}");
        assert!(
            running.contains(spinner_glyph(3)),
            "the working item carries the live spinner: {running:?}"
        );

        app.spinner_frame = 4;
        let next = render_todos_text(&app, 80);
        assert_ne!(running, next, "the row advances with the animation");
    }

    /// A list the model stopped maintaining is marked stale — also AFTER the turn
    /// ended (real report: the strip stayed on step 1 after a long run).
    #[test]
    fn the_strip_marks_an_unmaintained_list_as_stale() {
        let mut app = App::new();
        app.todos = vec![TodoView {
            content: "Alt".into(),
            status: "pending".into(),
        }];
        app.calls_since_todos = 5;
        let text = render_todos_text(&app, 80);
        assert!(!text.contains("(stale)"), "below the threshold: {text:?}");

        app.calls_since_todos = 6;
        let text = render_todos_text(&app, 80);
        assert!(text.contains("(stale)"), "{text:?}");

        // A real list change resets the counter -> no marker (idle or running).
        app.calls_since_todos = 0;
        let text = render_todos_text(&app, 80);
        assert!(!text.contains("(stale)"), "{text:?}");
    }

    /// Prints all views (visible with --nocapture) — proves that the
    /// look really is as intended.
    #[test]
    fn snapshot_frames_print_all_views() {
        for (title, frame) in snapshot_frames(92, 24, 128_000) {
            println!("\n── {title} ──\n{frame}");
        }
    }

    /// The activity row alone (live: spinner, phase, elapsed).
    fn render_activity_text(app: &App, width: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(width, 1)).expect("test terminal");
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height: 1,
        };
        term.draw(|f| render_activity(f, area, app))
            .expect("draw activity");
        buffer_text(term.backend().buffer(), width, 1)
    }

    /// The data row alone (measurements).
    fn render_data_text(app: &App, width: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(width, 1)).expect("test terminal");
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height: 1,
        };
        term.draw(|f| render_data(f, area, app)).expect("draw data");
        buffer_text(term.backend().buffer(), width, 1)
    }

    /// Both rows as they appear in the frame (activity above, data below).
    fn render_status_text(app: &App, width: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(width, 2)).expect("test terminal");
        let root = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(Rect {
            x: 0,
            y: 0,
            width,
            height: 2,
        });
        term.draw(|f| {
            render_activity(f, root[0], app);
            render_data(f, root[1], app);
        })
        .expect("draw status");
        buffer_text(term.backend().buffer(), width, 2)
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer, width: u16, height: u16) -> String {
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            out.push('\n');
        }
        out
    }

    /// Every tool line must show a readable summary. Previously all
    /// tools outside read_file/search_files/terminal/patch/write fell
    /// back to the raw JSON head ("{"data":{"web":[{"title":...") — exactly
    /// that is what this test checks for each of the 26 tool names.
    #[test]
    fn tool_done_lines_are_human_not_json() {
        let cases: Vec<(&str, &str, &str, &str)> = vec![
            // (tool, args_json, result_json, expected substring)
            (
                "process_manage",
                r#"{"action":"list"}"#,
                r#"{"count":2,"processes":[{"session_id":"proc_00000001","running":true}]}"#,
                "2 process",
            ),
            (
                "process_manage",
                r#"{"action":"poll","session_id":"proc_00000001"}"#,
                r#"{"new_output":"line-1\nline-2\n","new_stderr":""}"#,
                "2 new line(s)",
            ),
            (
                "process_manage",
                r#"{"action":"wait","session_id":"proc_00000001"}"#,
                r#"{"process":{"session_id":"proc_00000001","running":false,"exit_code":0}}"#,
                "terminated (exit 0)",
            ),
            (
                "process_manage",
                r#"{"action":"log","session_id":"proc_00000001"}"#,
                r#"{"output":"a\nb\n","total_lines":7}"#,
                "7 lines total",
            ),
            (
                "terminal",
                r#"{"command":"sleep 9","background":true}"#,
                r#"{"session_id":"proc_00000002","background":true}"#,
                "proc_2",
            ),
            (
                "terminal",
                r#"{"command":"echo hi"}"#,
                r#"{"output":"hi\n","exit_code":0}"#,
                "hi",
            ),
            (
                "web_search",
                r#"{"query":"hyprland"}"#,
                r#"{"provider":"websurfx","data":{"web":[{"title":"A"},{"title":"B"}]}}"#,
                "2 results (websurfx)",
            ),
            (
                "web_extract",
                r#"{"urls":["https://a.example/x"]}"#,
                r#"{"results":[{"url":"https://a.example/x","content":"..."}]}"#,
                "1 page(s)",
            ),
            (
                "arxiv",
                r#"{"action":"search","query":"transformers"}"#,
                r#"{"action":"search","count":2,"results":[{"title":"Attention Is All You Need"},{"title":"BERT"}]}"#,
                "Attention Is All You Need",
            ),
            (
                "brain_search",
                r#"{"query":"registry"}"#,
                r#"{"mode":"hybrid","hits":[{"source":"src/registry.rs","label":"Tool registry"}]}"#,
                "1 hit",
            ),
            (
                "context",
                r#"{"action":"recall","query":"registry"}"#,
                "Found 2 relevant memories\nregistry routes tool calls",
                "Found 2 relevant memories",
            ),
            (
                "mcp",
                r#"{"action":"list_tools","server":"docs"}"#,
                r#"{"server":"docs","count":3,"tools":[{"name":"search"},{"name":"fetch"},{"name":"open"}]}"#,
                "3 tools",
            ),
            (
                "github",
                r#"{"action":"repo_view","repo":"rijoda/rijo-code"}"#,
                "rijoda/rijo-code [private]\n  default: master",
                "rijoda/rijo-code",
            ),
            (
                "skills_list",
                r#"{}"#,
                r#"{"skills":[{"name":"a"},{"name":"b"},{"name":"c"}]}"#,
                "3 Skills",
            ),
            (
                "skill_view",
                r#"{"name":"rijo-rust-harness"}"#,
                r#"{"content":"z1\nz2\nz3","path":"/x/SKILL.md"}"#,
                "3 lines",
            ),
            (
                "skill_manage",
                r#"{"name":"x"}"#,
                r#"{"success":true,"path":"/x/SKILL.md"}"#,
                "SKILL.md",
            ),
            (
                "memory",
                r#"{"action":"add"}"#,
                r#"{"success":true,"current_chars":2181}"#,
                "2181 chars",
            ),
            (
                "todo_list",
                r#"{"todos":[{"id":1},{"id":2}]}"#,
                r#"{"todos":[{"id":1},{"id":2}]}"#,
                "2 tasks",
            ),
            (
                "session_search",
                r#"{"query":"websurfx"}"#,
                r#"{"results":[{"id":"s1"}]}"#,
                "1 session(s)",
            ),
            (
                "cronjob_manage",
                r#"{"action":"list"}"#,
                r#"{"jobs":[{"id":"j1"},{"id":"j2"}]}"#,
                "2 job(s)",
            ),
            (
                "execute_code",
                r#"{"code":"print(1)"}"#,
                r#"{"output":"42\n","exit_code":0}"#,
                "42",
            ),
            (
                "vision_analyze",
                r#"{"image_url":"/tmp/a.png"}"#,
                r#"{"content":"A blue triangle on a white background"}"#,
                "blue triangle",
            ),
            (
                "browser_navigate",
                r#"{"url":"https://x.example"}"#,
                r#"{"navigated":true,"title":"{\"frameId\":\"A\"}"}"#,
                "navigated",
            ),
            (
                "browser_snapshot",
                r#"{}"#,
                r#"{"content":"[IMAGE: x] text"}"#,
                "chars",
            ),
            (
                "browser_click",
                r#"{"ref":"e12"}"#,
                r#"{"clicked":true}"#,
                "clicked",
            ),
            (
                "browser_type",
                r#"{"ref":"e3","text":"hi"}"#,
                r#"{"typed":true}"#,
                "typed",
            ),
            (
                "browser_scroll",
                r#"{"direction":"down"}"#,
                r#"{"scrolled":true}"#,
                "scrolled",
            ),
            (
                "browser_dev",
                r#"{"action":"screenshot"}"#,
                r#"{"path":"C:/tmp/browser-1.png","bytes":2048}"#,
                "png · 2048",
            ),
            (
                "text_to_speech",
                r#"{"text":"hello"}"#,
                r#"{"backend":"espeak-ng","audio_path":"/home/u/.rijo/data/audio/a.wav","chars":5}"#,
                "espeak-ng",
            ),
            (
                "delegate_task",
                r#"{"goal":"x"}"#,
                r#"{"results":[{"ok":true}]}"#,
                "1 subagent",
            ),
            (
                "clarify",
                r#"{"question":"continue?"}"#,
                r#"{"answer":"yes"}"#,
                "yes",
            ),
            (
                "read_file",
                r#"{"path":"a.rs"}"#,
                r#"{"content":"x\ny","total_lines":2}"#,
                "2 lines",
            ),
            (
                "write_file",
                r#"{"path":"a.rs"}"#,
                r#"{"bytes_written":42}"#,
                "42 Bytes",
            ),
            (
                "patch",
                r#"{"path":"a.rs"}"#,
                r#"{"changes":3}"#,
                "3 changes",
            ),
            (
                "search_files",
                r#"{"pattern":"x"}"#,
                r#"{"count":5,"matches":"a\nb"}"#,
                "5 matches",
            ),
            (
                "delete_file",
                r#"{"path":"a.rs"}"#,
                r#"{"deleted":true}"#,
                "deleted",
            ),
            (
                "git",
                r#"{"action":"status"}"#,
                r#"{"branch":"main","clean":false,"counts":{"staged":0,"unstaged":2,"untracked":1}}"#,
                "main · 3 changed",
            ),
            (
                "brain_outline",
                r#"{"path":"."}"#,
                r#"{"file_count":3,"symbol_count":12}"#,
                "3 file",
            ),
        ];
        let covered: std::collections::HashSet<&str> =
            cases.iter().map(|(tool, _, _, _)| *tool).collect();
        let registry = rijo_core::tools::Registry::build_default();
        for name in registry.get_names() {
            assert!(
                covered.contains(name) || name.starts_with("box_"),
                "registered tool {name} needs an explicit result-summary case"
            );
        }
        for (tool, args, result, want) in cases {
            let got = smart_excerpt(tool, args, result);
            assert!(got.contains(want), "{tool}: expected {want:?}, got {got:?}");
            assert!(
                !got.contains('{') && !got.contains('"'),
                "{tool}: raw JSON blob in the display: {got:?}"
            );
        }
    }

    /// Errors stay errors — the message is the information.
    #[test]
    fn tool_errors_show_the_message() {
        let got = smart_excerpt(
            "process_manage",
            r#"{"action":"poll","session_id":"proc_9"}"#,
            r#"{"error":"unknown process: proc_9"}"#,
        );
        assert!(got.contains("unknown process: proc_9"), "{got}");

        // Blocked command: previously this said "ok", although the line was
        // marked as an error — the user saw a ✗ without any reason.
        let blocked = smart_excerpt(
            "terminal",
            r#"{"command":"rm -rf ./*"}"#,
            r#"{"error":"BLOCKED (recursive delete) — the user did not approve this terminal action"}"#,
        );
        assert!(blocked.contains("BLOCKED"), "{blocked}");
        assert!(
            !blocked.contains("ok"),
            "a blocked action must not show 'ok': {blocked}"
        );
    }

    /// github answers with plain text (no JSON envelope), so the excerpt has to
    /// fall back to the first line instead of showing the raw arg dump.
    #[test]
    fn github_excerpt_shows_the_result_line() {
        let got = smart_excerpt(
            "github",
            r#"{"action":"repo_view","repo":"rijoda/rijo-code"}"#,
            "rijoda/rijo-code  [private]\n  default:     master\n  url:         https://github.com/rijoda/rijo-code\n",
        );
        assert!(got.contains("rijoda/rijo-code  [private]"), "{got}");
        assert!(
            !got.contains("default:"),
            "only the headline, not the whole body: {got}"
        );

        // The error path stays on the generic branch and shows the message.
        let err = smart_excerpt(
            "github",
            r#"{"action":"repo_view"}"#,
            r#"{"error":"GitHub API error 404: Not Found"}"#,
        );
        assert!(err.contains("404"), "{err}");
    }

    /// Diagnostics from the turn path must not be printed to stderr while the TUI
    /// owns the terminal (a real report showed the context note inside the status
    /// row, with the layout shifted below it), and they are not shown as a popup
    /// either — the log file is the place for them.
    #[test]
    fn diagnostics_go_to_the_log_file_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = rijo_core::config::Config::default();
        config.data_dir = dir.path().to_path_buf();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let state = rijo_core::AppState {
            brain: std::sync::Arc::new(
                rijo_core::brain::db::BrainDb::open(&config.data_dir.join("brain.db"), 384)
                    .unwrap(),
            ),
            embedder: tokio::sync::Mutex::new(None),
            config,
            session_id: rijo_core::AppState::new_session_id(),
            tools: std::sync::Arc::new(rijo_core::tools::Registry::build_default()),
            ui_tx: std::sync::Mutex::new(Some(tx)),
            files_read: std::sync::Mutex::new(std::collections::HashSet::new()),
            cancel: std::sync::atomic::AtomicBool::new(false),
            router: std::sync::Mutex::new(None),
        };
        rijo_core::debug::note(&state, "context: 2 tool result(s) stubbed");
        // Nothing is drawn for it: no event, no toast (the popup in the right part
        // of the chat area was removed on request).
        assert!(
            rx.try_recv().is_err(),
            "diagnostics must not produce a TUI event"
        );
        let log = std::fs::read_to_string(dir.path().join("diagnostics.log")).unwrap();
        assert!(log.contains("stubbed"), "not logged: {log}");
    }

    /// While a turn runs, the context gauge shows the ESTIMATE of the request
    /// being sent (marked "~"): the provider reports its token count only with
    /// the answer, and on the FIRST turn there is no previous number at all — the
    /// user would otherwise see nothing new while waiting.
    #[test]
    fn running_line_shows_the_live_context_estimate() {
        let mut app = App::new();
        app.running = true;
        app.phase = "✎ writing…".to_string();
        app.live_ctx_chars = 48_000;
        app.live_ctx_messages = 12;
        app.turn_started = Some(std::time::Instant::now());

        // Without a known window: size only, still marked as an estimate.
        let text = render_status_text(&app, 140);
        assert!(text.contains("context ≈12k"), "{text}");
        assert!(
            !text.contains('%'),
            "no percentage without a window: {text}"
        );

        // With the window from the previous turn: estimate plus percentage.
        app.last_stats = Some(TurnStatsView {
            iterations: 1,
            llm_ms: 900,
            tool_ms: 0,
            total_ms: 900,
            tool_calls: 0,
            prompt_tokens: 4000,
            last_prompt_tokens: 4000,
            completion_tokens: 10,
            cache_hit: 0,
            cache_miss: 0,
            failures: 0,
            brain_hits: 0,
            ttft_ms: 500,
            context_window: 128_000,
        });
        let text = render_status_text(&app, 140);
        assert!(text.contains("context ≈12k of 128k (9%)"), "{text}");
        // The finished turn must NOT carry the estimate marker.
        app.running = false;
        let text = render_status_text(&app, 140);
        assert!(text.contains("context 4.0k of 128k (3%)"), "{text}");
    }

    /// Both rows exist at the same time while a turn runs: the activity row shows
    /// what is happening NOW, the data row keeps the measurements of the last
    /// finished turn (plus the live context estimate). Previously the single row
    /// switched between the two states, so the numbers vanished during a turn.
    #[test]
    fn both_rows_stay_visible_during_a_turn() {
        let mut app = App::new();
        app.running = true;
        app.phase = "⚙ terminal".to_string();
        app.tool_started = Some(std::time::Instant::now());
        app.turn_started = Some(std::time::Instant::now());
        app.live_ctx_chars = 48_000;
        app.context_window = 128_000;
        app.last_stats = Some(TurnStatsView {
            iterations: 2,
            llm_ms: 1100,
            tool_ms: 200,
            total_ms: 1400,
            tool_calls: 5,
            prompt_tokens: 44_400,
            last_prompt_tokens: 44_400,
            completion_tokens: 33,
            cache_hit: 3200,
            cache_miss: 800,
            failures: 0,
            brain_hits: 0,
            ttft_ms: 620,
            context_window: 128_000,
        });

        let both = render_status_text(&app, 160);
        let mut lines = both.lines();
        let activity = lines.next().unwrap();
        let data = lines.next().unwrap();

        // Row 1: live state.
        assert!(activity.contains("terminal"), "{activity}");
        assert!(activity.contains("Ctrl+C aborts"), "{activity}");
        assert!(
            !activity.contains("ctx"),
            "measurements belong below: {activity}"
        );
        // Row 2: measurements stay readable during the turn.
        assert!(data.contains("context ≈12k of 128k (9%)"), "{data}");
        assert!(data.contains("turn 1.4s"), "{data}");
        assert!(data.contains("first 620ms"), "{data}");
        assert!(!data.contains("ready"), "state belongs above: {data}");
    }

    /// Before the first turn the data row says so instead of staying blank (an
    /// empty row looks like a rendering bug and the layout jumps).
    #[test]
    fn data_row_explains_itself_before_the_first_turn() {
        let app = App::new();
        let data = render_data_text(&app, 120);
        assert!(data.contains("no measurements yet"), "{data}");

        let mut running = App::new();
        running.running = true;
        running.live_ctx_chars = 8_000;
        running.context_window = 128_000;
        let data = render_data_text(&running, 120);
        assert!(data.contains("context ≈2.0k of 128k (2%)"), "{data}");

        // The detected 1M window of a V4.x DeepSeek model must render as "1.0M".
        running.context_window = 1_048_576;
        let data = render_data_text(&running, 120);
        assert!(data.contains("context ≈2.0k of 1.0M (0%)"), "{data}");
        assert!(!data.contains("no measurements yet"), "{data}");
    }

    /// The last turn must be visible in the UI — previously the measurements
    /// only lived in turn_metrics.jsonl.
    #[test]
    fn status_line_shows_last_turn_stats() {
        let mut app = App::new();
        app.running = false;
        app.last_stats = Some(TurnStatsView {
            iterations: 3,
            llm_ms: 1100,
            tool_ms: 200,
            total_ms: 1400,
            tool_calls: 5,
            prompt_tokens: 4400,
            last_prompt_tokens: 4400,
            completion_tokens: 33,
            cache_hit: 3200,
            cache_miss: 800,
            failures: 0,
            brain_hits: 4,
            ttft_ms: 620,
            context_window: 128_000,
        });
        // Compact by default: context, latency, turn time, cache. Everything
        // else is one Ctrl+O away.
        let compact = render_status_text(&app, 200);
        assert!(compact.contains("ready"), "{compact}");
        assert!(compact.contains("context 4.4k of 128k (3%)"), "{compact}");
        assert!(compact.contains("first 620ms"), "{compact}");
        assert!(compact.contains("turn 1.4s"), "{compact}");
        assert!(compact.contains("cache 80%"), "{compact}");
        assert!(
            !compact.contains("LLM ") && !compact.contains("tok/s") && !compact.contains("brain "),
            "the full breakdown belongs behind Ctrl+O: {compact}"
        );

        // Full view (Ctrl+O): the whole breakdown.
        app.verbose = true;
        let text = render_status_text(&app, 200);
        assert!(text.contains("ready"), "{text}");
        assert!(text.contains("turn 1.4s"), "{text}");
        assert!(text.contains("LLM 1.1s"), "{text}");
        assert!(text.contains("Tools 200ms"), "{text}");
        assert!(text.contains("3×"), "{text}");
        assert!(text.contains("5 Tool"), "{text}");
        assert!(text.contains("→33 tok"), "{text}");
        // 3200 of the 4000 reported prompt tokens came from the cache
        assert!(text.contains("cache 80%"), "{text}");
        // context fill, first-token latency, throughput, brain.
        assert!(text.contains("context 4.4k of 128k (3%)"), "{text}");
        assert!(text.contains("first 620ms"), "{text}");
        assert!(text.contains("30 tok/s"), "{text}");
        assert!(text.contains("brain 4"), "{text}");
    }

    /// Priority on a narrow terminal: state and context survive, the extras and
    /// the hints go. A value is never cut in the middle.
    #[test]
    fn status_line_prioritises_on_a_narrow_terminal() {
        let mut app = App::new();
        app.running = false;
        app.session_turns = 3;
        app.session_prompt_tokens = 40_000;
        app.session_completion_tokens = 500;
        app.last_stats = Some(TurnStatsView {
            iterations: 3,
            llm_ms: 1100,
            tool_ms: 200,
            total_ms: 1400,
            tool_calls: 5,
            prompt_tokens: 44_400,
            last_prompt_tokens: 44_400,
            completion_tokens: 33,
            cache_hit: 3200,
            cache_miss: 800,
            failures: 2,
            brain_hits: 4,
            ttft_ms: 620,
            context_window: 128_000,
        });

        // The session totals are the least urgent value: they live in the full
        // view (Ctrl+O), not in the compact default.
        app.verbose = true;
        let wide = render_data_text(&app, 200);
        assert!(wide.contains("sess 3 turn(s)"), "{wide}");
        assert!(wide.contains("2 failed"), "{wide}");
        app.verbose = false;

        // The measurements live on their own row now, so the priority question is
        // about that row: context and ttft first, then the turn values.
        let narrow = render_data_text(&app, 80);
        assert!(
            !narrow.contains("ready"),
            "state lives on the row above: {narrow}"
        );
        assert!(narrow.contains("context 44k of 128k (35%)"), "{narrow}");
        assert!(narrow.contains("first 620ms"), "{narrow}");
        assert!(
            !narrow.contains("Ctrl+O"),
            "hints live on the row above: {narrow}"
        );
        // The narrow data row in one look (golden): context, TTFT, then the turn
        // values that still fit, cut on an item boundary.
        assert!(
            narrow.contains("context 44k of 128k (35%) ▰▰▱▱▱ · first 620ms …"),
            "{narrow}"
        );
        assert!(
            !narrow.contains("sess"),
            "session totals are lower priority: {narrow}"
        );
        // No half-written value at the right edge: a cut "Cache 2…" reads like
        // 2%, so whole items are dropped instead.
        for line in narrow.lines() {
            let trimmed = line.trim_end();
            assert!(!trimmed.ends_with("k/"), "value cut mid-number: {line:?}");
            assert!(
                !trimmed.ends_with("Cache 2…"),
                "half a percentage: {line:?}"
            );
            // A shortened list ends on "…", and the cut is always marked.
            if trimmed.contains('…') {
                assert!(trimmed.ends_with('…'), "unmarked cut: {line:?}");
            }
        }
        // A very narrow row cuts on an item boundary and marks the cut.
        let tight = render_data_text(&app, 50);
        assert!(tight.contains("context 44k of 128k (35%)"), "{tight}");
        assert!(tight.contains("…"), "the shortened list is marked: {tight}");
        assert!(
            !tight.contains("cache 3…") && !tight.contains("cache 4…"),
            "partial percentage in {tight}"
        );

        // The colour tells the same story as the number: 35% is not alarming.
        let style = ctx_segment(44_400, 128_000, false).unwrap().1;
        assert_eq!(style.fg, Some(theme::DIM));
        // 60-84% amber, >=85% red — thresholds are the whole point of the gauge.
        assert_eq!(
            ctx_segment(90_000, 128_000, false).unwrap().1.fg,
            Some(theme::WARN)
        );
        assert_eq!(
            ctx_segment(115_000, 128_000, false).unwrap().1.fg,
            Some(theme::ERR)
        );
        // The activity row carries the state and the hints, and nothing else.
        let activity = render_activity_text(&app, 120);
        assert!(activity.contains("ready"), "{activity}");
        assert!(activity.contains("Ctrl+O=details"), "{activity}");
        assert!(
            !activity.contains("ctx"),
            "measurements belong below: {activity}"
        );

        // A 1M window must not read as "1000k".
        assert_eq!(human_tokens(1_000_000), "1.0M");
        assert_eq!(human_tokens(1_048_576), "1.0M");
        assert_eq!(human_tokens(200_000), "200k");
        assert_eq!(human_tokens(4_400), "4.4k");

        // Unknown window: number without a made-up denominator.
        let (text, style) = ctx_segment(44_400, 0, false).unwrap();
        assert_eq!(text, "context 44k");
        assert_eq!(style.fg, Some(theme::DIM));
        assert!(
            ctx_segment(0, 128_000, false).is_none(),
            "no data = no segment"
        );

        // A running turn shows the estimate with the "~" marker; the finished
        // turn shows the provider-reported number without it.
        let estimated = ctx_segment(44_400, 128_000, true).unwrap().0;
        assert!(estimated.starts_with("context ≈"), "{estimated}");
        // The percentage is now followed by the fill gauge.
        assert!(estimated.contains("35%"), "{estimated}");
        assert!(estimated.ends_with("▰▰▱▱▱"), "{estimated}");
    }

    /// Without cache data the line stays honest (no invented "Cache 0%").
    #[test]
    fn status_line_omits_cache_when_provider_silent() {
        let mut app = App::new();
        app.last_stats = Some(TurnStatsView {
            iterations: 1,
            llm_ms: 900,
            tool_ms: 0,
            total_ms: 900,
            tool_calls: 0,
            prompt_tokens: 4402,
            last_prompt_tokens: 4402,
            completion_tokens: 2,
            cache_hit: 0,
            cache_miss: 0,
            failures: 0,
            brain_hits: 0,
            ttft_ms: 0,
            context_window: 128_000,
        });
        let text = render_status_text(&app, 110);
        assert!(text.contains("turn 900ms"), "{text}");
        assert!(!text.contains("cache"), "{text}");
    }

    /// Without a turn the line stays clean (no empty "Turn 0ms").
    #[test]
    fn status_line_without_stats_has_no_turn_garbage() {
        let app = App::new();
        let text = render_status_text(&app, 110);
        assert!(text.contains("ready"), "{text}");
        assert!(!text.contains("Turn"), "{text}");
    }

    /// The gauge fills proportionally: the shape and the colour tell the same
    /// story as the number next to it.
    #[test]
    fn the_gauge_fills_proportionally() {
        assert_eq!(gauge(0), "▱▱▱▱▱");
        assert_eq!(gauge(9), "▱▱▱▱▱");
        assert_eq!(gauge(35), "▰▰▱▱▱");
        assert_eq!(gauge(80), "▰▰▰▰▱");
        assert_eq!(gauge(100), "▰▰▰▰▰");
    }

    /// A left click on a status bar toggles that bar's view (same as Ctrl+O /
    /// Ctrl+T); a click anywhere else is left for the chat selection.
    #[test]
    fn a_click_on_a_status_row_toggles_that_view() {
        let mut app = App::new();
        app.last_row_activity = Some(10);
        app.last_row_todo = Some(12);

        assert!(
            toggle_status_row(&mut app, 10),
            "activity row consumes the click"
        );
        assert!(app.verbose, "activity click shows the full breakdown");
        assert!(
            toggle_status_row(&mut app, 12),
            "todo row consumes the click"
        );
        assert!(app.todos_expanded, "todo click expands the checklist");
        assert!(
            !toggle_status_row(&mut app, 5),
            "a click in the chat is not consumed"
        );
    }

    /// A tool entry must be readable in the rendered chat — and the
    /// full excerpt (Ctrl+O) must not lose the info.
    #[test]
    fn chat_renders_tool_line_readable() {
        let mut app = App::new();
        let body = format!(
            "✓ process_manage · wait proc_0000 · 1.2s{}",
            smart_excerpt(
                "process_manage",
                r#"{"action":"wait","session_id":"proc_00000001"}"#,
                r#"{"process":{"session_id":"proc_00000001","running":false,"exit_code":0}}"#
            )
        );
        app.entries.push(ChatEntry {
            role: EntryRole::Tool,
            text: body.clone(),
            detail: Some(entry_full_detail(
                "process_manage",
                true,
                1200,
                r#"{"process":{"session_id":"proc_00000001","running":false,"exit_code":0}}"#,
            )),
            expanded: false,
        });
        let mut term = Terminal::new(TestBackend::new(100, 6)).expect("term");
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 6,
        };
        term.draw(|f| render_chat(f, area, &mut app))
            .expect("draw chat");
        let text = buffer_text(term.backend().buffer(), 100, 6);
        assert!(text.contains("process_manage"), "{text}");
        assert!(text.contains("terminated (exit 0)"), "{text}");
        // The result sits UNDER its call, not beside it: the head line carries the
        // call, the following line the result behind a `│` edge.
        assert!(
            text.contains("│ terminated (exit 0)"),
            "the result must be on its own indented line: {text}"
        );
    }

    /// /help must name all commands — previously the two call sites listed
    /// different sets (the palette did not know /thinking).
    #[test]
    fn help_lists_every_command() {
        let h = help_line();
        for cmd in COMMANDS {
            if *cmd == "/quit" || *cmd == "/thinking" {
                continue; // /quit is an alias, /thinking is listed with usage
            }
            assert!(h.contains(cmd), "{cmd} missing in: {h}");
        }
        assert!(h.contains("/thinking min|med|max"), "{h}");
        assert!(h.contains("/metrics"), "{h}");
    }

    /// All palette commands must also be executable (no entry without a
    /// handler) — exactly this gap made /metrics and /thinking run into nothing.
    #[test]
    fn every_palette_command_has_a_handler() {
        for cmd in COMMANDS {
            // /update self-replaces the *running* binary — inside a test that
            // is this test executable (self_replace on current_exe). It is
            // covered by the pending_update/apply_update logic instead.
            // /paste reads the OS clipboard, which a test must not depend on;
            // its decision logic is covered by `paste_read` tests below.
            if *cmd == "/update" || *cmd == "/paste" {
                continue;
            }
            let mut app = App::new();
            execute_command(&mut app, cmd);
            // /help and /copy always produce a status line; /exit sets the flag
            if *cmd == "/exit" || *cmd == "/quit" {
                assert!(
                    QUIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst),
                    "{cmd} does not quit"
                );
                QUIT_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
            } else if *cmd == "/tools" {
                assert!(
                    app.tool_explorer.is_some(),
                    "/tools did not open the catalog"
                );
            } else if *cmd == "/runtime" {
                assert!(
                    app.runtime_scroll.is_some(),
                    "/runtime did not open the panel"
                );
            } else if *cmd == "/operations" {
                assert!(
                    app.operations_selected.is_some(),
                    "/operations did not open the panel"
                );
            } else if *cmd == "/history" {
                assert!(
                    app.history_view.is_some(),
                    "/history did not open the panel"
                );
            } else if *cmd == "/brain" {
                assert_eq!(
                    app.history_view.as_ref().map(|view| view.kind),
                    Some(SearchMode::Brain),
                    "/brain did not open workspace search"
                );
            } else if *cmd == "/skills" {
                assert!(app.skills_view.is_some(), "/skills did not open the panel");
            } else if *cmd == "/activity" {
                assert!(
                    app.activity_view.is_some(),
                    "/activity did not open archive search"
                );
            } else {
                assert!(!app.entries.is_empty(), "{cmd} has no handler (no output)");
            }
        }
    }
}
