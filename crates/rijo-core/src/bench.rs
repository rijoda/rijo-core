//! Benchmark harness — compares Rijo Code tool call latency vs Hermes (Python)

use serde_json::json;
use std::time::{Duration, Instant};

pub struct Benchmark {
    results: Vec<BenchResult>,
}

pub struct BenchResult {
    pub name: &'static str,
    pub rijo_us: u64,
    pub hermes_us: u64,
    pub ratio: f64,
}

impl Default for Benchmark {
    fn default() -> Self {
        Self::new()
    }
}

impl Benchmark {
    pub fn new() -> Self {
        Self {
            results: Vec::new(),
        }
    }

    /// Measure Rijo Code tool call latency (in-process)
    pub async fn measure_rijo<F, T>(&mut self, name: &'static str, f: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let start = Instant::now();
        let result = f.await;
        let elapsed = start.elapsed();
        tracing::info!("[BENCH] Rijo Code {}: {:?}", name, elapsed);
        result
    }

    /// Record from a batch of measured durations -> median rijo time
    pub fn record_rijo_batch(
        &mut self,
        name: &'static str,
        durations: &[Duration],
        hermes_us: u64,
    ) {
        let mut us: Vec<u64> = durations.iter().map(|d| d.as_micros() as u64).collect();
        us.sort_unstable();
        let median_us = us.get(us.len() / 2).copied().unwrap_or(0);
        let ratio = if hermes_us > 0 {
            hermes_us as f64 / median_us.max(1) as f64
        } else {
            0.0
        };
        self.results.push(BenchResult {
            name,
            rijo_us: median_us,
            hermes_us,
            ratio,
        });
    }

    /// Record a comparison with Hermes
    pub fn record_hermes(&mut self, name: &'static str, rijo_dur: Duration, hermes_dur: Duration) {
        let rijo_us = rijo_dur.as_micros() as u64;
        let hermes_us = hermes_dur.as_micros() as u64;
        let ratio = if hermes_us > 0 {
            hermes_us as f64 / rijo_us.max(1) as f64
        } else {
            0.0
        };
        self.results.push(BenchResult {
            name,
            rijo_us,
            hermes_us,
            ratio,
        });
    }

    pub fn print_summary(&self) {
        println!("\n=== RIJO vs HERMES BENCHMARK ===");
        println!(
            "{:<25} {:>12} {:>12} {:>10}",
            "Tool", "Rijo Code (μs)", "Hermes (μs)", "Speedup"
        );
        println!("{}", "-".repeat(61));
        for r in &self.results {
            let hermes_disp = if r.hermes_us == 0 {
                "not measured".to_string()
            } else {
                format!("{:>12}", r.hermes_us)
            };
            let ratio_disp = if r.hermes_us == 0 {
                "n/a".to_string()
            } else {
                format!("{:>8.1}x", r.ratio)
            };
            println!(
                "{:<25} {:>12} {} {}",
                r.name, r.rijo_us, hermes_disp, ratio_disp
            );
        }
        println!("{}", "-".repeat(61));
    }
}

/// Headless benchmark runner for `rijo --bench`.
///
/// Measures core tools in-process (warmup + median) and compares against
/// `data/hermes_times.json` (measured via `data/measure_hermes.py`).
/// Paths are absolute under the workspace so results don't depend on cwd.
pub async fn run_bench(state: std::sync::Arc<crate::AppState>) -> Result<(), String> {
    use std::time::Instant;

    let ws = state.config.workspace.clone();
    let hermes_path = ws.join("data/hermes_times.json");
    let hermes_raw = std::fs::read_to_string(&hermes_path).map_err(|e| {
        format!(
            "Hermes baseline missing ({}): {e} — re-measure via python3 data/measure_hermes.py",
            hermes_path.display()
        )
    })?;
    let hermes: serde_json::Value =
        serde_json::from_str(&hermes_raw).map_err(|e| format!("hermes_times.json invalid: {e}"))?;
    let h = |key: &str| hermes.get(key).and_then(|v| v.as_u64()).unwrap_or(0);

    let bench_file = ws.join(".bench_tmp").to_string_lossy().to_string();
    let cargo_toml = ws.join("Cargo.toml").to_string_lossy().to_string();
    let crates_dir = ws.join("crates").to_string_lossy().to_string();
    let cases: Vec<(&str, &str, &str, serde_json::Value)> = vec![
        (
            "read_file",
            "read_file",
            "read_file_us",
            json!({"path": cargo_toml}),
        ),
        (
            "write_file_small",
            "write_file",
            "write_file_small_us",
            json!({"path": bench_file, "content": "bench0123456789abcdef".repeat(8)}),
        ),
        (
            "search_files",
            "search_files",
            "search_files_us",
            json!({"pattern": "Registry", "target": "content", "path": crates_dir, "output_mode": "count"}),
        ),
        ("skills_list", "skills_list", "skills_list_us", json!({})),
        (
            "terminal_echo",
            "terminal",
            "terminal_echo_us",
            json!({"command": "echo bench"}),
        ),
    ];

    const WARMUP: usize = 2;
    const ITERS: usize = 7;
    let mut bench = Benchmark::new();
    for (label, tool, hkey, args) in &cases {
        eprintln!("[bench] start {label}");
        for _ in 0..WARMUP {
            state
                .tools
                .dispatch(tool, args.clone(), state.clone())
                .await;
        }
        let mut durs = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let t = Instant::now();
            let res = state
                .tools
                .dispatch(tool, args.clone(), state.clone())
                .await;
            durs.push(t.elapsed());
            if res.starts_with("{\"error\":") {
                eprintln!(
                    "[bench] {tool} error: {}",
                    res.chars().take(200).collect::<String>()
                );
            }
        }
        eprintln!("[bench] done {label}");
        bench.record_rijo_batch(label, &durs, h(hkey));
    }
    // Cleanup directly via fs (delete_file needs a TUI confirm)
    let _ = std::fs::remove_file(ws.join(".bench_tmp"));
    bench.print_summary();

    // ── Parallel batch: 8 read-only calls in one turn ────────────────────
    // Same code-path function as the executor (run_parallel_batch) against
    // strictly sequential execution — that is the speedup for multi-tool turns.
    let batch: Vec<(String, serde_json::Value)> = (0..8)
        .map(|_| {
            (
                "search_files".to_string(),
                json!({"pattern": "Registry", "target": "content", "path": crates_dir, "output_mode": "count"}),
            )
        })
        .collect();

    let t = Instant::now();
    for _ in 0..3 {
        for (name, args) in &batch {
            state
                .tools
                .dispatch(name, args.clone(), state.clone())
                .await;
        }
    }
    let serial = t.elapsed() / 3;

    let t = Instant::now();
    for _ in 0..3 {
        crate::executor::run_parallel_batch(
            &state,
            batch.clone(),
            state.config.max_concurrent_tools,
            false,
        )
        .await;
    }
    let parallel = t.elapsed() / 3;

    let speedup = serial.as_secs_f64() / parallel.as_secs_f64().max(f64::EPSILON);
    println!("\n=== PARALLEL BATCH (8 read-only calls, one turn) ===");
    println!(
        "sequential (old):   {:>8.2} ms",
        serial.as_secs_f64() * 1000.0
    );
    println!(
        "parallel   (new):   {:>8.2} ms",
        parallel.as_secs_f64() * 1000.0
    );
    println!(
        "Speedup:            {:>8.2}x  (Concurrency-Limit {})",
        speedup, state.config.max_concurrent_tools
    );

    // ── Netz-Batch (opt-in: RIJO_BENCH_NET=1) ────────────────────────────
    // Network latency is the largest single item; explicitly opt-in here
    // so the default bench stays deterministic and offline.
    if std::env::var("RIJO_BENCH_NET").as_deref() == Ok("1") {
        let urls = [
            "https://example.com",
            "https://example.org",
            "https://www.rust-lang.org",
            "https://doc.rust-lang.org/std/",
        ];
        let net_batch: Vec<(String, serde_json::Value)> = urls
            .iter()
            .map(|u| ("web_extract".to_string(), json!({"urls": [u]})))
            .collect();

        let t = Instant::now();
        for (name, args) in &net_batch {
            state
                .tools
                .dispatch(name, args.clone(), state.clone())
                .await;
        }
        let net_serial = t.elapsed();

        let t = Instant::now();
        crate::executor::run_parallel_batch(
            &state,
            net_batch.clone(),
            state.config.max_concurrent_tools,
            false,
        )
        .await;
        let net_parallel = t.elapsed();

        println!("\n=== NET BATCH (4 web_extract, one turn) ===");
        println!(
            "sequential (old):   {:>8.2} ms",
            net_serial.as_secs_f64() * 1000.0
        );
        println!(
            "parallel   (new):   {:>8.2} ms",
            net_parallel.as_secs_f64() * 1000.0
        );
        println!(
            "Speedup:            {:>8.2}x",
            net_serial.as_secs_f64() / net_parallel.as_secs_f64().max(f64::EPSILON)
        );
    }
    // ── Measured runs: startup phases + turn breakdown ────────────────────
    // Source is data_dir/turn_metrics.jsonl (real runs, not an estimate).
    let m = crate::metrics::summarize(&state.config.data_dir);
    println!("\n=== MEASURED RUNS (turn_metrics.jsonl) ===");
    println!(
        "{}",
        serde_json::to_string_pretty(&m).unwrap_or_else(|_| m.to_string())
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::executor::is_parallel_safe;

    #[test]
    fn read_only_tools_are_parallel_safe() {
        for t in [
            "read_file",
            "search_files",
            "skills_list",
            "skill_view",
            "session_search",
            "web_search",
            "web_extract",
        ] {
            assert!(is_parallel_safe(t), "{t} should be batch-capable");
        }
    }

    #[test]
    fn mutating_and_stateful_tools_are_not_parallel_safe() {
        // browser_* share ONE tab — navigate/snapshot/screenshot would race.
        for t in [
            "write_file",
            "patch",
            "delete_file",
            "terminal",
            "execute_code",
            "delegate_task",
            "browser_navigate",
            "browser_snapshot",
            "browser_click",
            "browser_type",
            "browser_scroll",
            "memory",
            "skill_manage",
            "todo_list",
            "cronjob_manage",
            "vision_analyze",
            "text_to_speech",
        ] {
            assert!(!is_parallel_safe(t), "{t} must NOT run in parallel");
        }
    }
}
