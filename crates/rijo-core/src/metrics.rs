//! Metrics: startup phases and turn breakdown as JSONL.
//!
//! Purpose: "maximally fast" should have a measured number. Without that every
//! statement about latency is a claim. One line per turn with
//! LLM time / tool time / iterations / tokens — plus the startup phases.
//!
//! Append is serialized process-wide (parallel tool batches would otherwise
//! write into each other, as with the tool trace).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static METRICS_LOCK: Mutex<()> = Mutex::new(());

pub fn metrics_path(data_dir: &Path) -> PathBuf {
    data_dir.join("turn_metrics.jsonl")
}

/// Appends a record (best-effort, never an error for the caller).
pub fn append(data_dir: &Path, record: serde_json::Value) {
    let _ = std::fs::create_dir_all(data_dir);
    let _guard = METRICS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(metrics_path(data_dir))
    {
        let _ = f.write_all(format!("{record}\n").as_bytes());
    }
}

/// Measure and record a startup phase: `kind: "startup"`.
pub fn startup_phase(data_dir: &Path, phase: &str, ms: u128, extra: serde_json::Value) {
    let mut rec = serde_json::json!({
        "kind": "startup",
        "ts": chrono::Utc::now().to_rfc3339(),
        "phase": phase,
        "ms": ms as u64,
    });
    if let (Some(obj), Some(e)) = (rec.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            obj.insert(k.clone(), v.clone());
        }
    }
    append(data_dir, rec);
}

/// A turn record (`kind: "turn"`). As a struct, because nine
/// positional arguments had to be counted at every call site — with
/// cache/size fields that otherwise becomes unreadable.
#[derive(Debug, Clone, Default)]
pub struct TurnRecord {
    pub iterations: usize,
    pub llm_ms: u128,
    pub tool_ms: u128,
    pub total_ms: u128,
    pub tool_calls: usize,
    pub prompt_tokens: u32,
    /// Prompt tokens of the last request (the real context fill); `prompt_tokens`
    /// is the sum over all iterations (billed volume).
    pub last_prompt_tokens: u64,
    pub completion_tokens: u32,
    /// Provider cache hit (DeepSeek prefix caching); 0 = not reported.
    pub cache_hit: u32,
    pub cache_miss: u32,
    /// Time to first streamed token of the turn in ms (0 = nothing streamed).
    /// The number the user feels first: a slow TTFT looks like a hang while the
    /// tokens-per-second rate still looks fine afterwards.
    pub ttft_ms: u128,
    /// Tool calls of this turn that came back as an error.
    pub failures: usize,
    /// Characters (not tokens) of system prompt and tool schemas — shows
    /// what the fixed prompt overhead per turn consists of.
    pub sys_chars: usize,
    pub tools_chars: usize,
    pub source: Option<String>,
    /// Brain lookup of this turn: how long it took, how many hits survived the
    /// score floor, the best score, and which path found them ("vector" or
    /// "fulltext" when the embedder was busy). Without these numbers any claim
    /// about the brain's value is guesswork.
    pub brain_ms: u128,
    pub brain_hits: usize,
    pub brain_top_score: f64,
    pub brain_mode: Option<String>,
}

pub fn turn(data_dir: &Path, rec: &TurnRecord) {
    append(
        data_dir,
        serde_json::json!({
            "kind": "turn",
            "ts": chrono::Utc::now().to_rfc3339(),
            "iterations": rec.iterations,
            "llm_ms": rec.llm_ms as u64,
            "tool_ms": rec.tool_ms as u64,
            "overhead_ms": (rec.total_ms.saturating_sub(rec.llm_ms + rec.tool_ms)) as u64,
            "total_ms": rec.total_ms as u64,
            "tool_calls": rec.tool_calls,
            "prompt_tokens": rec.prompt_tokens,
            "last_prompt_tokens": rec.last_prompt_tokens,
            "completion_tokens": rec.completion_tokens,
            "cache_hit_tokens": rec.cache_hit,
            "cache_miss_tokens": rec.cache_miss,
            "ttft_ms": rec.ttft_ms as u64,
            "failures": rec.failures,
            "sys_chars": rec.sys_chars,
            "tools_chars": rec.tools_chars,
            "brain_ms": rec.brain_ms as u64,
            "brain_hits": rec.brain_hits,
            "brain_top_score": (rec.brain_top_score * 1000.0).round() / 1000.0,
            "brain_mode": rec.brain_mode.clone().unwrap_or_else(|| "off".into()),
            "source": rec.source.as_deref().unwrap_or("interactive"),
        }),
    );
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx]
}

/// Summarizes the measurement file: per kind/phase count, median, p95.
pub fn summarize(data_dir: &Path) -> serde_json::Value {
    let Ok(content) = std::fs::read_to_string(metrics_path(data_dir)) else {
        return serde_json::json!({ "records": 0, "hint": "no metrics yet — run once with --e2e or --query" });
    };

    let mut starts: std::collections::BTreeMap<String, Vec<u64>> = Default::default();
    let mut turns: std::collections::BTreeMap<String, Vec<u64>> = Default::default();
    let mut turn_count = 0usize;
    let mut tokens = (0u64, 0u64);
    let mut cache = (0u64, 0u64);
    let mut fixed = (0u64, 0u64); // (sys_chars, tools_chars) — constant, hence max
    let mut iterations_total = 0u64;

    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
            "startup" => {
                if let (Some(p), Some(ms)) = (
                    v.get("phase").and_then(|p| p.as_str()),
                    v.get("ms").and_then(|m| m.as_u64()),
                ) {
                    starts.entry(p.to_string()).or_default().push(ms);
                }
            }
            "turn" => {
                turn_count += 1;
                iterations_total += v.get("iterations").and_then(|x| x.as_u64()).unwrap_or(0);
                tokens.0 += v.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                tokens.1 += v
                    .get("completion_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                cache.0 += v
                    .get("cache_hit_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                cache.1 += v
                    .get("cache_miss_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                if let Some(sc) = v.get("sys_chars").and_then(|x| x.as_u64()) {
                    fixed.0 = fixed.0.max(sc); // system prompt is constant
                }
                if let Some(tc) = v.get("tools_chars").and_then(|x| x.as_u64()) {
                    fixed.1 = fixed.1.max(tc); // schemas are constant
                }
                for key in ["total_ms", "llm_ms", "tool_ms", "overhead_ms"] {
                    if let Some(ms) = v.get(key).and_then(|x| x.as_u64()) {
                        turns.entry(key.to_string()).or_default().push(ms);
                    }
                }
            }
            _ => {}
        }
    }

    let summarize_map = |m: &std::collections::BTreeMap<String, Vec<u64>>| -> serde_json::Value {
        let mut out = serde_json::Map::new();
        for (k, vals) in m {
            let mut sorted = vals.clone();
            sorted.sort_unstable();
            out.insert(
                k.clone(),
                serde_json::json!({
                    "n": sorted.len(),
                    "median": percentile(&sorted, 0.5),
                    "p95": percentile(&sorted, 0.95),
                    "max": sorted.last().copied().unwrap_or(0),
                }),
            );
        }
        serde_json::Value::Object(out)
    };

    serde_json::json!({
        "records": turn_count,
        "turns": summarize_map(&turns),
        "startup_phases": summarize_map(&starts),
        "avg_iterations_per_turn": if turn_count > 0 {
            (iterations_total as f64 / turn_count as f64 * 100.0).round() / 100.0
        } else { 0.0 },
        "tokens": { "prompt": tokens.0, "completion": tokens.1 },
        "cache": {
            "hit": cache.0,
            "miss": cache.1,
            // Share of prompt tokens from the provider cache. Zero reports
            // => field is missing (provider delivers no cache statistics).
            "hit_ratio": if cache.0 + cache.1 > 0 {
                ((cache.0 as f64 / (cache.0 + cache.1) as f64) * 1000.0).round() / 1000.0
            } else { 0.0 },
        },
        "fixed_prompt_chars": { "system": fixed.0, "tools": fixed.1 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rijo-metrics-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn appends_and_summarizes() {
        let d = tmp("sum");
        startup_phase(&d, "executor_init", 42, serde_json::json!({}));
        for total in [100u128, 200, 300] {
            turn(
                &d,
                &TurnRecord {
                    iterations: 2,
                    llm_ms: 50,
                    tool_ms: 20,
                    total_ms: total,
                    tool_calls: 3,
                    prompt_tokens: 1000,
                    last_prompt_tokens: 800,
                    completion_tokens: 250,
                    cache_hit: 800,
                    cache_miss: 200,
                    ttft_ms: 410,
                    failures: 1,
                    sys_chars: 9000,
                    tools_chars: 12000,
                    source: Some("query".into()),
                    brain_ms: 7,
                    brain_hits: 3,
                    brain_top_score: 0.71,
                    brain_mode: Some("vector".into()),
                },
            );
        }
        let s = summarize(&d);
        assert_eq!(s["records"], 3);
        assert_eq!(s["turns"]["total_ms"]["median"], 200, "{s}");
        assert_eq!(s["turns"]["total_ms"]["n"], 3);
        assert_eq!(s["startup_phases"]["executor_init"]["median"], 42);
        assert_eq!(s["tokens"]["prompt"], 3000);
        assert_eq!(s["tokens"]["completion"], 750);
        assert_eq!(s["avg_iterations_per_turn"], 2.0);
        // Cache sums and ratio (3x 800 hit / 200 miss = 80 %)
        assert_eq!(s["cache"]["hit"], 2400, "{s}");
        assert_eq!(s["cache"]["miss"], 600);
        assert_eq!(s["cache"]["hit_ratio"], 0.8);
        // fixed prompt share: constant, hence max instead of sum
        assert_eq!(s["fixed_prompt_chars"]["system"], 9000);
        assert_eq!(s["fixed_prompt_chars"]["tools"], 12000);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let d = tmp("empty");
        let s = summarize(&d);
        assert_eq!(s["records"], 0);
        assert!(s.get("hint").is_some());
    }
}
