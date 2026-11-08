//! Schema evaluation: how far can the tool descriptions be shortened without
//! the tool choice getting worse?
//!
//! Why this has to be measured: with ~12,000 characters the tool schemas are the
//! largest part of the fixed prompt overhead per turn (measured: 4,402 prompt
//! tokens, system prompt 3,545 characters vs. schemas 12,041 characters).
//! So every shortening saves on EVERY turn — but an overly terse description
//! costs the wrong tool choice, and that costs a whole roundtrip.
//!
//! Structure: the same task catalog runs against several shortening variants,
//! each task multiple times, at temperature 0. Measured per variant:
//!   * characters/tokens of the schema (saving)
//!   * hit rate of the tool choice (quality)
//!   * argument hit rate (quality in detail)
//!   * latency and provider cache hits
//!
//! Nothing here is executed — it is only about the model's decision.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::llm::{ChatMessage, LlmClient};

// ─── Task catalog ─────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Task {
    pub id: String,
    pub prompt: String,
    /// Acceptable tools. Several are intentional: there are often several ways to the goal.
    pub accept: Vec<String>,
    /// Substrings that must appear in the arguments of the chosen call.
    #[serde(default)]
    pub required_args: Vec<String>,
    #[serde(default)]
    pub category: String,
    /// true = an answer without a tool call is also correct.
    #[serde(default)]
    pub no_tool_ok: bool,
    /// Legitimate FIRST step that is not yet the goal: the model may look first
    /// (fetch the process id, element snapshot, check a file) and act afterwards
    /// in the real loop. Scored strictly this counts as a miss — this list makes
    /// visible how many misses are of this kind.
    #[serde(default)]
    pub precursor: Vec<String>,
    /// The same for the case that the tool is right but a different action value
    /// came (e.g. `action: list` instead of `kill`, because the id is missing).
    /// If this text appears in the arguments, the run counts as a documented
    /// intermediate step instead of a miss.
    #[serde(default)]
    pub precursor_args: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct TaskFile {
    tasks: Vec<Task>,
}

pub fn load_tasks(path: &Path) -> Result<Vec<Task>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("task catalog {} not readable: {e}", path.display()))?;
    let file: TaskFile =
        serde_json::from_str(&raw).map_err(|e| format!("task catalog invalid: {e}"))?;
    if file.tasks.is_empty() {
        return Err("task catalog is empty".into());
    }
    Ok(file.tasks)
}

// ─── Shortening variants ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Unchanged — the baseline for comparison.
    Full,
    /// Remove only the emoji prefix ("[🖥️] …").
    NoEmoji,
    /// Shorten the description to the first sentence (max. 90 characters).
    FirstSentence,
    /// Remove parameter descriptions (types/required stay).
    NoParamDesc,
    /// All together: no emoji, first sentence (max. 120), no parameter texts.
    Compact,
    /// Only name + required parameters with type — the most aggressive variant.
    Minimal,
}

impl Mode {
    pub fn name(&self) -> &'static str {
        match self {
            Mode::Full => "full",
            Mode::NoEmoji => "no_emoji",
            Mode::FirstSentence => "first_sentence",
            Mode::NoParamDesc => "no_param_desc",
            Mode::Compact => "compact",
            Mode::Minimal => "minimal",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" | "basis" => Some(Mode::Full),
            "no_emoji" | "noemoji" => Some(Mode::NoEmoji),
            "first_sentence" | "sentence" => Some(Mode::FirstSentence),
            "no_param_desc" | "noparamdesc" => Some(Mode::NoParamDesc),
            "compact" => Some(Mode::Compact),
            "minimal" => Some(Mode::Minimal),
            _ => None,
        }
    }

    pub fn all() -> Vec<Mode> {
        vec![
            Mode::Full,
            Mode::NoEmoji,
            Mode::FirstSentence,
            Mode::NoParamDesc,
            Mode::Compact,
            Mode::Minimal,
        ]
    }

    /// Variants for a quick run (baseline + the two candidates that are
    /// realistically in question).
    pub fn default_set() -> Vec<Mode> {
        vec![Mode::Full, Mode::NoEmoji, Mode::Compact]
    }
}

/// "[🖥️] Execute a shell command…" → "Execute a shell command…"
fn strip_emoji_prefix(desc: &str) -> String {
    let t = desc.trim_start();
    if let Some(rest) = t.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            // Emoji prefixes are short; anything beyond that is real text
            if end <= 6 {
                return rest[end + 1..].trim_start().to_string();
            }
        }
    }
    desc.to_string()
}

/// First sentence, hard-limited to `cap` characters (at a word boundary).
fn first_sentence(desc: &str, cap: usize) -> String {
    let d = desc.trim();
    let end = d
        .find(". ")
        .map(|i| i + 1)
        .or_else(|| d.find(".\n"))
        .unwrap_or(d.len());
    let sentence = d[..end].trim();
    truncate_words(sentence, cap)
}

fn truncate_words(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let cut: String = s.chars().take(cap).collect();
    match cut.rfind(' ') {
        Some(i) if i > cap / 2 => format!("{}…", cut[..i].trim_end()),
        _ => format!("{}…", cut.trim_end()),
    }
}

/// Removes the "description" field from all parameters.
fn strip_param_descriptions(params: &mut Value) {
    if let Some(props) = params.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for (_, spec) in props.iter_mut() {
            if let Some(obj) = spec.as_object_mut() {
                obj.remove("description");
            }
        }
    }
}

/// Keeps only required parameters with their type (and enum, which is half the info).
fn minimal_params(params: &Value) -> Value {
    let mut out = json!({ "type": "object", "properties": {} });
    let required: Vec<String> = params
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if !required.is_empty() {
        out["required"] = json!(required);
    }
    if let Some(props) = params.get("properties").and_then(|p| p.as_object()) {
        for name in &required {
            if let Some(spec) = props.get(name) {
                let mut keep =
                    json!({ "type": spec.get("type").cloned().unwrap_or(json!("string")) });
                if let Some(e) = spec.get("enum") {
                    keep["enum"] = e.clone();
                }
                out["properties"][name] = keep;
            }
        }
    }
    out
}

/// Shortens a complete schema set according to `mode`. Returns the new schemas;
/// the order stays stable (the provider cache rewards stable prefixes).
pub fn compact_schemas(schemas: &[Value], mode: Mode) -> Vec<Value> {
    schemas
        .iter()
        .map(|schema| {
            let mut s = schema.clone();
            let Some(func) = s.get_mut("function").and_then(|f| f.as_object_mut()) else {
                return s;
            };
            let original = func
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            match mode {
                Mode::Full => {}
                Mode::NoEmoji => {
                    func.insert("description".into(), json!(strip_emoji_prefix(&original)));
                }
                Mode::FirstSentence => {
                    func.insert("description".into(), json!(first_sentence(&original, 90)));
                }
                Mode::NoParamDesc => {
                    if let Some(p) = func.get_mut("parameters") {
                        strip_param_descriptions(p);
                    }
                }
                Mode::Compact => {
                    let short = first_sentence(&strip_emoji_prefix(&original), 120);
                    func.insert("description".into(), json!(short));
                    if let Some(p) = func.get_mut("parameters") {
                        strip_param_descriptions(p);
                    }
                }
                Mode::Minimal => {
                    let short = first_sentence(&strip_emoji_prefix(&original), 60);
                    func.insert("description".into(), json!(short));
                    if let Some(p) = func.get("parameters") {
                        let mini = minimal_params(p);
                        func.insert("parameters".into(), mini);
                    }
                }
            }
            s
        })
        .collect()
}

/// Character count of a schema set (the measure for the overhead).
pub fn schemas_chars(schemas: &[Value]) -> usize {
    serde_json::to_string(schemas)
        .map(|s| s.chars().count())
        .unwrap_or(0)
}

/// Breakdown per tool: total characters, of which description, of which parameters.
/// Shows which few tools dominate the overhead — that is where shortening pays off.
pub fn per_tool_report(schemas: &[Value]) -> String {
    let mut rows: Vec<(String, usize, usize)> = schemas
        .iter()
        .filter_map(|s| {
            let f = s.get("function")?;
            let name = f.get("name")?.as_str()?.to_string();
            let desc = f
                .get("description")
                .and_then(|d| d.as_str())
                .map(|d| d.chars().count())
                .unwrap_or(0);
            let params = f
                .get("parameters")
                .map(|p| {
                    serde_json::to_string(p)
                        .map(|s| s.chars().count())
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            Some((name, desc, params))
        })
        .collect();
    rows.sort_by_key(|(_, d, p)| std::cmp::Reverse(d + p));
    let total: usize = rows.iter().map(|(_, d, p)| d + p).sum();
    let mut out = format!(
        "{:<18}{:>9}{:>9}{:>9}{:>8}\n",
        "Tool", "total", "Desc.", "Params", "Share"
    );
    out.push_str(&"-".repeat(53));
    out.push('\n');
    for (name, desc, params) in &rows {
        let sum = desc + params;
        out.push_str(&format!(
            "{name:<18}{sum:>9}{desc:>9}{params:>9}{:>7.0}%\n",
            if total > 0 {
                sum as f64 / total as f64 * 100.0
            } else {
                0.0
            }
        ));
    }
    out.push_str(&format!("\nTotal schema chars: {total}\n"));

    // How much is in the 5 largest tools?
    let top5: usize = rows.iter().take(5).map(|(_, d, p)| d + p).sum();
    if total > 0 {
        out.push_str(&format!(
            "The 5 heaviest tools: {top5} chars = {:.0}% of the overhead\n",
            top5 as f64 / total as f64 * 100.0
        ));
    }
    out
}

// ─── Scoring ───────────────────────────────────────────────────────────

/// A single tool call, reduced to what is scored.
#[derive(Debug, Clone)]
pub struct Call {
    pub name: String,
    pub args: String,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub tool_ok: bool,
    pub args_ok: bool,
    /// First step is a documented intermediate step (not a miss, but also not
    /// yet the goal).
    pub precursor_only: bool,
    pub called: Vec<String>,
    pub note: String,
}

impl Outcome {
    pub fn fully_ok(&self) -> bool {
        self.tool_ok && self.args_ok
    }
}

pub fn score(task: &Task, calls: &[Call]) -> Outcome {
    if calls.is_empty() {
        return Outcome {
            tool_ok: task.no_tool_ok,
            args_ok: task.no_tool_ok,
            precursor_only: false,
            called: vec![],
            note: if task.no_tool_ok {
                "no tool call (allowed)".into()
            } else {
                "no tool call".into()
            },
        };
    }
    let called: Vec<String> = calls.iter().map(|c| c.name.clone()).collect();
    let hit = calls
        .iter()
        .find(|c| task.accept.iter().any(|a| a == &c.name));
    let Some(hit) = hit else {
        // Maybe just an intermediate step (looking instead of acting)?
        let is_precursor = calls
            .iter()
            .all(|c| task.precursor.iter().any(|p| p == &c.name));
        let got: String = calls
            .iter()
            .map(|c| format!("{} {}", c.name, truncate_words(&c.args, 60)))
            .collect::<Vec<_>>()
            .join(" | ");
        return Outcome {
            tool_ok: false,
            args_ok: false,
            precursor_only: is_precursor && !task.precursor.is_empty(),
            called,
            note: if is_precursor && !task.precursor.is_empty() {
                format!(
                    "intermediate step ({got}) — target would be {}",
                    task.accept.join("|")
                )
            } else {
                format!("wrong tool: {got} (expected: {})", task.accept.join("|"))
            },
        };
    };
    // Arguments: all required substrings must appear (case-insensitive, so that
    // spellings like "Hyprland"/"hyprland" do not count as an error).
    let args_lower = hit.args.to_ascii_lowercase();
    let missing: Vec<&String> = task
        .required_args
        .iter()
        .filter(|need| !args_lower.contains(&need.to_ascii_lowercase()))
        .collect();
    Outcome {
        tool_ok: true,
        args_ok: missing.is_empty(),
        // Tool is right, but a different action value came — is that a
        // documented intermediate step (id still unknown)?
        precursor_only: !missing.is_empty()
            && !task.precursor_args.is_empty()
            && task
                .precursor_args
                .iter()
                .any(|p| args_lower.contains(&p.to_ascii_lowercase())),
        called,
        note: if missing.is_empty() {
            String::new()
        } else {
            // Do not just say "missing", but show what actually came —
            // often it is a different enum value (list instead of poll).
            format!(
                "missing expected value: {} (got: {})",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                truncate_words(&hit.args, 60)
            )
        },
    }
}

// ─── Run ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EvalOptions {
    pub tasks_path: PathBuf,
    pub modes: Vec<Mode>,
    pub repeats: usize,
    pub out_dir: PathBuf,
    /// Only the first N tasks (for quick runs).
    pub limit: Option<usize>,
    /// Subset the tools per task with the router (gate E2: does picking fewer
    /// schemas cost tool-pick accuracy?). Orthogonal to `Mode` — a run with
    /// `Mode::Full` + router is the direct A/B against `Mode::Full` alone.
    pub router: bool,
}

impl Default for EvalOptions {
    fn default() -> Self {
        Self {
            tasks_path: PathBuf::from("data/schema_tasks.json"),
            modes: Mode::default_set(),
            repeats: 2,
            out_dir: PathBuf::from("."),
            limit: None,
            router: false,
        }
    }
}

/// Result of a variant.
#[derive(Debug, Clone)]
pub struct ModeResult {
    pub mode: Mode,
    pub schema_chars: usize,
    pub task_runs: usize,
    pub tool_ok: usize,
    pub args_ok: usize,
    /// Runs in which the first step was a documented intermediate step
    /// (e.g. `list` first, to learn the process id). Counts strictly as a miss,
    /// but is not a fault of the description.
    pub precursor_only: usize,
    pub cache_hit: u64,
    pub cache_miss: u64,
    pub prompt_tokens: u64,
    pub latencies_ms: Vec<u64>,
    pub failures: Vec<String>,
    /// Router on/off for this run.
    pub router: bool,
    /// Sum of the schema characters actually sent per task run (with router
    /// this varies per task — that is the whole point of the gate).
    pub sent_chars_total: usize,
    /// Router latency per task run, milliseconds.
    pub router_ms: Vec<u64>,
    /// Turns where the router proposed nothing and the full list was sent.
    pub router_fallbacks: usize,
}

impl ModeResult {
    pub fn tool_accuracy(&self) -> f64 {
        if self.task_runs == 0 {
            0.0
        } else {
            self.tool_ok as f64 / self.task_runs as f64
        }
    }
    pub fn args_accuracy(&self) -> f64 {
        if self.task_runs == 0 {
            0.0
        } else {
            self.args_ok as f64 / self.task_runs as f64
        }
    }
    /// Mean schema characters actually sent per task run (router runs).
    pub fn sent_chars_per_run(&self) -> f64 {
        if self.task_runs == 0 {
            0.0
        } else {
            self.sent_chars_total as f64 / self.task_runs as f64
        }
    }

    pub fn median_latency(&self) -> u64 {
        let mut v = self.latencies_ms.clone();
        if v.is_empty() {
            return 0;
        }
        v.sort_unstable();
        v[v.len() / 2]
    }
    pub fn cache_ratio(&self) -> Option<f64> {
        let seen = self.cache_hit + self.cache_miss;
        if seen == 0 {
            None
        } else {
            Some(self.cache_hit as f64 / seen as f64)
        }
    }
}

/// Runs the evaluation. `system` is the real system prompt (same conditions as
/// in operation), `temperature` is set to 0 so that the variants are comparable.
pub async fn run(
    llm: &LlmClient,
    tasks: &[Task],
    base_schemas: &[Value],
    system: &str,
    opts: &EvalOptions,
    mut router: Option<&mut crate::router::RouterSession>,
) -> Result<(Vec<ModeResult>, Value), String> {
    let mut results = Vec::new();
    let mut detail = Vec::new();
    let use_router = opts.router && router.is_some();

    for mode in &opts.modes {
        let schemas = compact_schemas(base_schemas, *mode);
        let chars = schemas_chars(&schemas);
        let mut res = ModeResult {
            mode: *mode,
            schema_chars: chars,
            task_runs: 0,
            tool_ok: 0,
            args_ok: 0,
            precursor_only: 0,
            cache_hit: 0,
            cache_miss: 0,
            prompt_tokens: 0,
            latencies_ms: Vec::new(),
            failures: Vec::new(),
            router: use_router,
            sent_chars_total: 0,
            router_ms: Vec::new(),
            router_fallbacks: 0,
        };

        for task in tasks {
            // Router subset for this task; on any miss the full (mode-reduced)
            // list goes out, exactly like a real turn.
            let mut task_schemas = schemas.clone();
            let mut predicted: Vec<String> = Vec::new();
            let mut fallback = true;
            let mut router_ms = 0u64;
            if use_router {
                let session = router.as_deref_mut().expect("checked above");
                let out = session.score(&task.prompt);
                router_ms = out.router_ms;
                if !out.tools.is_empty() {
                    let keep: std::collections::HashSet<&str> =
                        out.tools.iter().map(|(n, _)| n.as_str()).collect();
                    let subset: Vec<Value> = schemas
                        .iter()
                        .filter(|s| {
                            s["function"]["name"]
                                .as_str()
                                .map(|n| keep.contains(n))
                                .unwrap_or(false)
                        })
                        .cloned()
                        .collect();
                    if !subset.is_empty() && subset.len() < schemas.len() {
                        task_schemas = subset;
                        predicted = out.tools.iter().map(|(n, _)| n.clone()).collect();
                        fallback = false;
                    }
                }
                if fallback {
                    res.router_fallbacks += 1;
                }
            }
            let sent_chars = schemas_chars(&task_schemas);
            for rep in 0..opts.repeats.max(1) {
                let messages = vec![
                    ChatMessage {
                        role: "system".into(),
                        content: Some(system.to_string()),
                        reasoning_content: None,
                        tool_calls: None,
                        tool_call_id: None,
                        parts: None,
                    },
                    ChatMessage {
                        role: "user".into(),
                        content: Some(task.prompt.clone()),
                        reasoning_content: None,
                        tool_calls: None,
                        tool_call_id: None,
                        parts: None,
                    },
                ];
                let t0 = std::time::Instant::now();
                let out = llm.chat(&messages, &task_schemas).await;
                let ms = t0.elapsed().as_millis() as u64;
                res.latencies_ms.push(ms);
                res.sent_chars_total += sent_chars;
                if use_router {
                    res.router_ms.push(router_ms);
                }
                match out {
                    Ok((_text, _reasoning, calls, usage)) => {
                        if let Some(u) = usage {
                            res.prompt_tokens += u.prompt as u64;
                            res.cache_hit += u.cache_hit as u64;
                            res.cache_miss += u.cache_miss as u64;
                        }
                        let simplified: Vec<Call> = calls
                            .iter()
                            .map(|c| Call {
                                name: c.function.name.clone(),
                                args: c.function.arguments.clone(),
                            })
                            .collect();
                        let oc = score(task, &simplified);
                        res.task_runs += 1;
                        if oc.tool_ok {
                            res.tool_ok += 1;
                        }
                        if oc.args_ok && oc.tool_ok {
                            res.args_ok += 1;
                        }
                        if oc.precursor_only {
                            res.precursor_only += 1;
                        }
                        if !oc.fully_ok() {
                            res.failures.push(format!(
                                "{}#{}: {} — {}",
                                task.id,
                                rep + 1,
                                oc.called.join("+"),
                                oc.note
                            ));
                        }
                        detail.push(json!({
                            "mode": mode.name(),
                            "router": use_router,
                            "task": task.id,
                            "category": task.category,
                            "repeat": rep + 1,
                            "called": oc.called,
                            "tool_ok": oc.tool_ok,
                            "args_ok": oc.args_ok,
                            "precursor_only": oc.precursor_only,
                            "note": oc.note,
                            "ms": ms,
                            "router_ms": router_ms,
                            "router_fallback": fallback,
                            "router_predicted": predicted,
                            "sent_chars": sent_chars,
                            "args_raw": simplified.iter().map(|c| c.args.clone()).collect::<Vec<_>>(),
                        }));
                    }
                    Err(e) => {
                        res.failures
                            .push(format!("{}#{}: API error: {e}", task.id, rep + 1));
                        detail.push(json!({
                            "mode": mode.name(),
                            "task": task.id,
                            "repeat": rep + 1,
                            "error": e,
                            "ms": ms,
                        }));
                    }
                }
            }
        }
        results.push(res);
    }

    let report = json!({
        "kind": "schema_eval",
        "ts": chrono::Utc::now().to_rfc3339(),
        "tasks": tasks.len(),
        "repeats": opts.repeats,
        "base_schema_chars": schemas_chars(base_schemas),
        "modes": results.iter().map(|r| json!({
            "mode": r.mode.name(),
            "router": r.router,
            "schema_chars": r.schema_chars,
            "sent_chars_per_run": r.sent_chars_per_run(),
            "router_ms_p95": percentile95(&r.router_ms),
            "router_fallbacks": r.router_fallbacks,
            "runs": r.task_runs,
            "tool_ok": r.tool_ok,
            "tool_accuracy": r.tool_accuracy(),
            "args_ok": r.args_ok,
            "args_accuracy": r.args_accuracy(),
            "precursor_only": r.precursor_only,
            "median_ms": r.median_latency(),
            "prompt_tokens": r.prompt_tokens,
            "cache_hit": r.cache_hit,
            "cache_miss": r.cache_miss,
            "failures": r.failures,
        })).collect::<Vec<_>>(),
        "detail": detail,
    });
    Ok((results, report))
}

/// p95 of a latency sample (nearest-rank). Empty input → 0.
pub fn percentile95(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = ((sorted.len() as f64) * 0.95).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Readable table: saving vs. quality side by side.
pub fn render_table(results: &[ModeResult], base_chars: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<15} {:>9} {:>10} {:>12} {:>12} {:>9} {:>8}\n",
        "Variant", "Chars", "Saved", "Tool choice", "Args", "Median", "Cache"
    ));
    out.push_str(&"-".repeat(80));
    out.push('\n');
    for r in results {
        let saved = if base_chars > 0 {
            format!(
                "{:.0}%",
                (1.0 - r.schema_chars as f64 / base_chars as f64) * 100.0
            )
        } else {
            "—".into()
        };
        let cache = match r.cache_ratio() {
            Some(x) => format!("{:.0}%", x * 100.0),
            None => "—".into(),
        };
        out.push_str(&format!(
            "{:<15} {:>9} {:>10} {:>12} {:>12} {:>9} {:>8}\n",
            r.mode.name(),
            r.schema_chars,
            saved,
            format!(
                "{}/{} ({:.0}%)",
                r.tool_ok,
                r.task_runs,
                r.tool_accuracy() * 100.0
            ),
            format!(
                "{}/{} ({:.0}%)",
                r.args_ok,
                r.task_runs,
                r.args_accuracy() * 100.0
            ),
            format!("{}ms", r.median_latency()),
            cache,
        ));
    }
    // Report intermediate steps separately: they count strictly as a miss,
    // but are not a fault of the description.
    let any_pre = results.iter().any(|r| r.precursor_only > 0);
    if any_pre {
        out.push_str("\nOf which legitimate intermediate steps (look first, then act):\n");
        for r in results {
            if r.precursor_only > 0 {
                out.push_str(&format!(
                    "  {:<15} {}/{} runs\n",
                    r.mode.name(),
                    r.precursor_only,
                    r.task_runs
                ));
            }
        }
    }
    // Router runs report what was actually sent per task (varies per prompt),
    // plus the latency and fallback rate the E3 gate looks at.
    let router_runs: Vec<&ModeResult> = results.iter().filter(|r| r.router).collect();
    if !router_runs.is_empty() {
        out.push_str("\nRouter (tools actually sent per task):\n");
        for r in router_runs {
            let saved = if base_chars > 0 {
                format!(
                    "{:.0}%",
                    (1.0 - r.sent_chars_per_run() / base_chars as f64) * 100.0
                )
            } else {
                "—".into()
            };
            out.push_str(&format!(
                "  {:<15} sent {:>7} chars/run ({:>5} saved) · router p95 {}ms · fallbacks {}/{}\n",
                r.mode.name(),
                r.sent_chars_per_run() as usize,
                saved,
                percentile95(&r.router_ms),
                r.router_fallbacks,
                r.task_runs,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests run with cwd = crate root (crates/rijo-core); the catalog lives in
    /// the repo root. Resolve it via CARGO_MANIFEST_DIR so that the test does not
    /// depend on the calling environment.
    fn catalog_path() -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/schema_tasks.json")
    }

    /// The catalog must cover every registered tool at least once — otherwise the
    /// suite does not measure a shortening for all tools.
    #[test]
    fn task_catalog_covers_every_registered_tool() {
        const REGISTERED: &[&str] = &[
            "browser_click",
            "browser_navigate",
            "browser_scroll",
            "browser_snapshot",
            "browser_type",
            "clarify",
            "cronjob_manage",
            "delegate_task",
            "delete_file",
            "execute_code",
            "memory",
            "patch",
            "process_manage",
            "read_file",
            "search_files",
            "session_search",
            "skill_manage",
            "skill_view",
            "skills_list",
            "terminal",
            "text_to_speech",
            "todo_list",
            "vision_analyze",
            "web_extract",
            "web_search",
            "write_file",
        ];
        let tasks = load_tasks(&catalog_path()).expect("catalog readable");
        for tool in REGISTERED {
            assert!(
                tasks.iter().any(|t| t.accept.iter().any(|a| a == tool)),
                "no task covers {tool} — the suite would not check its description"
            );
        }
    }

    fn sample_schemas() -> Vec<Value> {
        vec![
            json!({
                "type": "function",
                "function": {
                    "name": "terminal",
                    "description": "[💻] Execute a shell command. Returns stdout, stderr, and exit code. Supports foreground (default) and background modes.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": {"type": "string", "description": "The shell command to run"},
                            "background": {"type": "boolean", "description": "Run in background and return a session id"}
                        },
                        "required": ["command"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "[📖] Read a text file with line numbers and pagination.",
                    "parameters": {
                        "type": "object",
                        "properties": {"path": {"type": "string", "description": "File path"}},
                        "required": ["path"]
                    }
                }
            }),
        ]
    }

    #[test]
    fn emoji_prefix_is_removed_only_when_short() {
        assert_eq!(strip_emoji_prefix("[💻] Run it"), "Run it");
        // real text in brackets stays
        assert_eq!(
            strip_emoji_prefix("[this is a real bracketed sentence] keep"),
            "[this is a real bracketed sentence] keep"
        );
    }

    #[test]
    fn first_sentence_stops_at_sentence_end() {
        let s = "Execute a shell command. Returns stdout and stderr.";
        assert_eq!(first_sentence(s, 90), "Execute a shell command.");
        // long sentences are truncated at a word boundary
        let long = "W".repeat(200);
        assert!(first_sentence(&long, 40).chars().count() <= 41);
    }

    #[test]
    fn every_mode_keeps_names_and_required_params() {
        let base = sample_schemas();
        let base_chars = schemas_chars(&base);
        for mode in Mode::all() {
            let out = compact_schemas(&base, mode);
            assert_eq!(out.len(), base.len(), "{mode:?} loses tools");
            for (a, b) in base.iter().zip(out.iter()) {
                assert_eq!(a["function"]["name"], b["function"]["name"], "{mode:?}");
                let req = a["function"]["parameters"]["required"].clone();
                let req2 = b["function"]["parameters"]["required"].clone();
                assert_eq!(req, req2, "{mode:?} loses required parameters");
                // required parameters must be preserved with their type
                if let Some(names) = req.as_array() {
                    for n in names.iter().filter_map(|x| x.as_str()) {
                        assert!(
                            b["function"]["parameters"]["properties"].get(n).is_some(),
                            "{mode:?} loses parameter {n}"
                        );
                    }
                }
            }
            assert!(
                schemas_chars(&out) <= base_chars,
                "{mode:?} makes the schemas larger"
            );
        }
    }

    #[test]
    fn compact_is_smaller_than_full_and_keeps_models_intent_readable() {
        let base = sample_schemas();
        let full = schemas_chars(&compact_schemas(&base, Mode::Full));
        let compact = schemas_chars(&compact_schemas(&base, Mode::Compact));
        let minimal = schemas_chars(&compact_schemas(&base, Mode::Minimal));
        assert_eq!(full, schemas_chars(&base), "full must not change anything");
        assert!(
            compact < full,
            "compact must be smaller: {compact} vs {full}"
        );
        assert!(
            minimal < compact,
            "minimal must be smaller: {minimal} vs {compact}"
        );
        // The shortening must keep the core of the description
        let c = compact_schemas(&base, Mode::Compact);
        let desc = c[0]["function"]["description"].as_str().unwrap();
        assert!(desc.starts_with("Execute a shell command"), "{desc}");
    }

    #[test]
    fn scoring_accepts_alternatives_and_checks_args() {
        let task = Task {
            id: "t".into(),
            prompt: "p".into(),
            accept: vec!["search_files".into(), "terminal".into()],
            required_args: vec!["websurfx".into()],
            category: "".into(),
            no_tool_ok: false,
            precursor: vec![],
            precursor_args: vec![],
        };
        let good = score(
            &task,
            &[Call {
                name: "terminal".into(),
                args: "{\"command\":\"grep websurfx -r\"}".into(),
            }],
        );
        assert!(good.fully_ok(), "{good:?}");
        // right tool, but the argument is missing
        let missing_arg = score(
            &task,
            &[Call {
                name: "terminal".into(),
                args: "{\"command\":\"grep -r foo\"}".into(),
            }],
        );
        assert!(
            missing_arg.tool_ok && !missing_arg.args_ok,
            "{missing_arg:?}"
        );
        // wrong tool
        let wrong = score(
            &task,
            &[Call {
                name: "read_file".into(),
                args: "{}".into(),
            }],
        );
        assert!(!wrong.tool_ok, "{wrong:?}");
        // no tool call at all
        let none = score(&task, &[]);
        assert!(!none.tool_ok && !none.args_ok);
    }

    #[test]
    fn groesse_ist_case_insensitive_geprueft() {
        let task = Task {
            id: "t".into(),
            prompt: "p".into(),
            accept: vec!["web_search".into()],
            required_args: vec!["hyprland".into()],
            category: "".into(),
            no_tool_ok: false,
            precursor: vec![],
            precursor_args: vec![],
        };
        let oc = score(
            &task,
            &[Call {
                name: "web_search".into(),
                args: "{\"query\":\"Hyprland Release\"}".into(),
            }],
        );
        assert!(oc.fully_ok(), "{oc:?}");
    }

    /// A documented intermediate step must not look like a wrong tool —
    /// otherwise the measurement counts legitimate looking as an error.
    #[test]
    fn dokumentierter_zwischenschritt_wird_als_solcher_erkannt() {
        let task = Task {
            id: "process_kill".into(),
            prompt: "End the process".into(),
            accept: vec!["process_manage".into()],
            required_args: vec!["kill".into()],
            category: "prozess".into(),
            no_tool_ok: false,
            precursor: vec![],
            precursor_args: vec!["list".into()],
        };
        // list first (to learn the id) — tool is right, target value did not come
        let pre = score(
            &task,
            &[Call {
                name: "process_manage".into(),
                args: "{\"action\":\"list\"}".into(),
            }],
        );
        assert!(
            pre.precursor_only,
            "must be recognized as an intermediate step: {pre:?}"
        );
        assert!(pre.tool_ok, "the tool is chosen correctly");
        assert!(!pre.args_ok, "the target value (kill) did not come");
        assert!(!pre.fully_ok());
        // The message must show what actually came
        assert!(pre.note.contains("list"), "{pre:?}");
        // Without the precursor_args rule the same run would be a plain miss
        let strict = Task {
            precursor_args: vec![],
            ..task.clone()
        };
        assert!(
            !score(
                &strict,
                &[Call {
                    name: "process_manage".into(),
                    args: "{\"action\":\"list\"}".into()
                }]
            )
            .precursor_only
        );
        // A documented intermediate step as the tool stays recognizable
        let other = Task {
            accept: vec!["write_file".into()],
            precursor: vec!["search_files".into()],
            ..task.clone()
        };
        let pre2 = score(
            &other,
            &[Call {
                name: "search_files".into(),
                args: "{\"pattern\":\"*\"}".into(),
            }],
        );
        assert!(pre2.precursor_only && !pre2.tool_ok, "{pre2:?}");
        assert!(pre2.note.contains("intermediate step"), "{pre2:?}");
        // a really wrong tool stays wrong
        let wrong = score(
            &task,
            &[Call {
                name: "web_search".into(),
                args: "{\"query\":\"x\"}".into(),
            }],
        );
        assert!(!wrong.precursor_only && !wrong.tool_ok, "{wrong:?}");
        // and an error message must show what actually came
        assert!(wrong.note.contains("web_search"), "{wrong:?}");
    }

    #[test]
    fn katzen_katalog_ist_gueltig() {
        let tasks = load_tasks(&catalog_path()).expect("catalog readable");
        assert!(tasks.len() >= 20, "too few tasks: {}", tasks.len());
        for t in &tasks {
            assert!(!t.accept.is_empty(), "{} has no acceptable tool", t.id);
        }
    }
}
