//! Runtime side of the router: tokenizer + ONNX session + calibrated scoring.
//!
//! Everything here is best-effort. A missing model file, a broken graph, a
//! tokenization error — any of it yields `Ok(None)` from [`RouterSession::load`]
//! or an empty score, and the caller sends the full tool set. The router may
//! only ever cost time, never behaviour.
//!
//! **ONNX Runtime loading.** The `ort` crate is built with `load-dynamic`,
//! which loads `onnxruntime.dll` at runtime via `libloading` instead of
//! linking a prebuilt at build time. This is why the crate builds on
//! `x86_64-pc-windows-gnu`: `ort-sys` ships prebuilts for eight targets,
//! `x86_64-pc-windows-msvc` among them but not gnu. With `load-dynamic` the
//! DLL is resolved at process start (see `ensure_ort_dll_loaded` below), so
//! the build is platform-independent.
//!
//! Without the DLL at runtime, `ort` returns an error and the caller falls
//! back to the full tool list — the same invariant as a missing bundle.

use std::path::Path;

use serde_json::Value;
use tokenizers::Tokenizer;

use super::router_model_dir;

/// Scores of one turn: per-tool sigmoid-free logits plus the auxiliary heads.
/// `tools` is sorted by score descending, already truncated to `max_tools`
/// and filtered by `min_score`.
#[derive(Debug, Clone, Default)]
pub struct RouterOutput {
    /// (tool name, score) above threshold, best first.
    pub tools: Vec<(String, f32)>,
    /// best toolset and its score (logit over the toolset head).
    pub toolset: Option<(String, f32)>,
    /// (class name, score) of the guard head, best first.
    pub guard: Vec<(String, f32)>,
    /// best compaction class and its score.
    pub compact: Option<(String, f32)>,
    /// wall-clock cost of one forward pass, milliseconds.
    pub router_ms: u64,
    /// number of tools the model proposed before filtering
    pub n_proposed: usize,
}

/// A loaded router bundle: ONNX graph, tokenizer and the label tables.
pub struct RouterSession {
    session: ort::session::Session,
    tokenizer: Tokenizer,
    tools: Vec<String>,
    toolsets: Vec<String>,
    guard: Vec<String>,
    compact: Vec<String>,
    /// Per-tool calibrated thresholds (from thresholds.json). A tool absent
    /// from the map uses `min_score`.
    thresholds: std::collections::HashMap<String, f32>,
    min_score: f32,
    max_tools: usize,
    max_len: usize,
    /// Temperature fitted on the synthetic eval split. Divides the raw logit
    /// before the sigmoid — monotone, so the ranking (and the recall) cannot
    /// change, only how the probabilities line up against a fixed threshold.
    temperature: f32,
}

/// Ensures the ONNX Runtime DLL can be found by `ort` at runtime.
///
/// `ort` with `load-dynamic` loads `onnxruntime.dll` at run time (env var
/// `ORT_DYLIB_PATH`, else the library search path). We point the variable at
/// the directory of the running executable, so placing `onnxruntime.dll` next
/// to `rijo.exe` is enough — no installer step, no PATH pollution.
///
/// The candidate list covers the cargo layout: a test binary lives in
/// `target/debug/deps/` while the DLL is copied to `target/debug/`. All
/// candidates are tried; if none exists the variable is left unset, so `ort`
/// keeps its own lookup instead of failing on a path we invented.
///
/// Idempotent: a variable that is already set (tests, embedders, CI) wins.
/// Where a runtime DLL may live, in the order we trust it.
///
/// The Tauri bundle puts `resources/` next to the exe on Windows, the CLI
/// installer uses `<rijo_home>/bin`, and dev/test builds keep it in `target/`.
/// All of them are checked, so one binary works in every layout.
fn ort_candidates(exe_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut candidates = vec![exe_dir.join("onnxruntime.dll")];
    if let Some(parent) = exe_dir.parent() {
        candidates.push(parent.join("onnxruntime.dll"));
    }
    candidates.push(exe_dir.join("resources").join("onnxruntime.dll"));
    candidates.push(
        crate::paths::rijo_home()
            .join("bin")
            .join("onnxruntime.dll"),
    );
    candidates
}

pub(crate) fn ensure_ort_dll_loaded() {
    // A variable that is *set but empty* must not count as configured: some
    // installers write it that way, and `is_some()` then silently skipped the
    // DLL next to the binary — the process fell back to a random system
    // `onnxruntime.dll` (measured 2026-10-29: 1.17.1 on PATH vs 1.28.0 bundled).
    if std::env::var_os("ORT_DYLIB_PATH").is_some_and(|value| !value.is_empty()) {
        return;
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let Some(exe_dir) = exe_dir else { return };
    for candidate in ort_candidates(&exe_dir) {
        if candidate.is_file() {
            std::env::set_var("ORT_DYLIB_PATH", &candidate);
            return;
        }
    }
}

/// Sigmoid on a slice; the tools head is multi-label so scores are per-tool
/// probabilities, not a softmax.
fn sigmoid(v: &[f32]) -> Vec<f32> {
    v.iter().map(|x| 1.0 / (1.0 + (-x).exp())).collect()
}

fn softmax(v: &[f32]) -> Vec<f32> {
    let max = v.iter().cloned().fold(f32::MIN, f32::max);
    let exps: Vec<f32> = v.iter().map(|x| (x - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    if sum <= 0.0 {
        return vec![0.0; v.len()];
    }
    exps.iter().map(|x| x / sum).collect()
}

fn read_labels(dir: &Path, file: &str) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(dir.join(file)).ok()?;
    let values: Value = serde_json::from_str(&text).ok()?;
    Some(
        values
            .as_array()?
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
    )
}

impl RouterSession {
    /// Load the bundle for `version` from [`router_model_dir`]. Returns `Ok(None)`
    /// when the bundle is absent or unreadable — the caller falls back to the
    /// full tool list.
    pub fn load(version: &str) -> Result<Option<Self>, String> {
        Self::load_from(&router_model_dir(version))
    }

    /// Same as [`RouterSession::load`] but against an explicit directory (tests).
    pub fn load_from(dir: &Path) -> Result<Option<Self>, String> {
        ensure_ort_dll_loaded();

        let model = dir.join("model.onnx");
        let tok_path = dir.join("tokenizer.json");
        if !model.exists() || !tok_path.exists() {
            return Ok(None);
        }
        let tools = read_labels(dir, "labels_tools.json").unwrap_or_default();
        let toolsets = read_labels(dir, "labels_toolsets.json").unwrap_or_default();
        let guard = read_labels(dir, "labels_guard.json").unwrap_or_default();
        let compact = read_labels(dir, "labels_compact.json").unwrap_or_default();
        if tools.is_empty() {
            return Err("labels_tools.json missing or empty".into());
        }
        let tokenizer = Tokenizer::from_file(&tok_path)
            .map_err(|e| format!("tokenizer {}: {e}", tok_path.display()))?;
        let mut tokenizer = tokenizer;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: 256,
                ..Default::default()
            }))
            .map_err(|e| format!("truncation: {e}"))?;

        let session = ort::session::Session::builder()
            .map_err(|e| format!("ort builder: {e}"))?
            .commit_from_file(&model)
            .map_err(|e| format!("onnx {}: {e}", model.display()))?;

        let (min_score, max_tools, max_len) = read_config(dir);
        let (thresholds, thr_min, thr_max, temperature) = read_thresholds(dir);
        Ok(Some(Self {
            session,
            tokenizer,
            tools,
            toolsets,
            guard,
            compact,
            thresholds,
            // thresholds.json is the calibrated artefact and wins over the
            // config defaults; config.json stays the fallback for old bundles.
            min_score: thr_min.unwrap_or(min_score),
            max_tools: thr_max.unwrap_or(max_tools),
            max_len,
            temperature: temperature.unwrap_or(1.0),
        }))
    }

    /// Override the model bundle's tool cap from the operator config.
    ///
    /// The calibrated bundle carries its own `max_tools`; without this setter the
    /// app config field was a silent no-op (measured 2026-10-18: 8 vs 6 sent the
    /// same 13 tools). `0` keeps the bundle value.
    pub fn set_max_tools(&mut self, max_tools: usize) {
        if max_tools > 0 {
            self.max_tools = max_tools;
        }
    }

    /// Score one prompt. Never fails: every error path returns a default
    /// (empty) output so the caller keeps sending every tool.
    pub fn score(&mut self, text: &str) -> RouterOutput {
        let started = std::time::Instant::now();
        let mut out = RouterOutput::default();
        let encoding = match self.tokenizer.encode(text, true) {
            Ok(e) => e,
            Err(_) => return out,
        };
        let ids: Vec<i64> = encoding.get_ids().iter().map(|i| *i as i64).collect();
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|i| *i as i64)
            .collect();
        if ids.is_empty() {
            return out;
        }
        let len = ids.len();
        let (shape, id_data, mask_data) = (vec![1i64, len as i64], ids, mask);
        let id_tensor = match ort::value::Tensor::from_array((shape.clone(), id_data)) {
            Ok(t) => t,
            Err(_) => return out,
        };
        let mask_tensor = match ort::value::Tensor::from_array((shape, mask_data)) {
            Ok(t) => t,
            Err(_) => return out,
        };
        let outputs = match self.session.run(ort::inputs![
            "input_ids" => id_tensor,
            "attention_mask" => mask_tensor
        ]) {
            Ok(o) => o,
            Err(_) => return out,
        };

        if let Ok((_, tool_logits)) = outputs["tools"].try_extract_tensor::<f32>() {
            let temp = if self.temperature > 0.0 {
                self.temperature
            } else {
                1.0
            };
            let probs = sigmoid(&tool_logits.iter().map(|x| x / temp).collect::<Vec<_>>());
            let mut ranked: Vec<(String, f32)> = probs
                .iter()
                .zip(self.tools.iter())
                .map(|(p, name)| (name.clone(), *p))
                .collect();
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let threshold_of = |name: &str| -> f32 {
                self.thresholds.get(name).copied().unwrap_or(self.min_score)
            };
            out.n_proposed = ranked
                .iter()
                .filter(|(name, p)| *p >= threshold_of(name))
                .count();
            out.tools = ranked
                .into_iter()
                .filter(|(name, p)| *p >= threshold_of(name))
                .take(self.max_tools)
                .collect();
        }
        if let Ok((_, ts_logits)) = outputs["toolset"].try_extract_tensor::<f32>() {
            let probs = softmax(ts_logits);
            if let Some((i, p)) = probs
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            {
                if let Some(name) = self.toolsets.get(i) {
                    out.toolset = Some((name.clone(), *p));
                }
            }
        }
        if let Ok((_, g_logits)) = outputs["guard"].try_extract_tensor::<f32>() {
            let probs = softmax(g_logits);
            let mut ranked: Vec<(String, f32)> = probs
                .iter()
                .zip(self.guard.iter())
                .map(|(p, name)| (name.clone(), *p))
                .collect();
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            out.guard = ranked;
        }
        if let Ok((_, c_logits)) = outputs["compact"].try_extract_tensor::<f32>() {
            let probs = softmax(c_logits);
            if let Some((i, p)) = probs
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            {
                if let Some(name) = self.compact.get(i) {
                    out.compact = Some((name.clone(), *p));
                }
            }
        }
        out.router_ms = started.elapsed().as_millis() as u64;
        out
    }

    pub fn tool_names(&self) -> &[String] {
        &self.tools
    }

    pub fn max_len(&self) -> usize {
        self.max_len
    }
}

fn read_config(dir: &Path) -> (f32, usize, usize) {
    let text = match std::fs::read_to_string(dir.join("config.json")) {
        Ok(t) => t,
        Err(_) => return (0.25, 8, 256),
    };
    let cfg: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return (0.25, 8, 256),
    };
    (
        cfg["min_score"].as_f64().unwrap_or(0.25) as f32,
        cfg["max_tools"].as_u64().unwrap_or(8) as usize,
        cfg["max_len"].as_u64().unwrap_or(256) as usize,
    )
}

/// Calibrated thresholds (optional): `{"per_tool": {...}, "max_tools": n,
/// "min_score": f, "temperature": f}` from `evaluate_router.py --write-thresholds`.
fn read_thresholds(
    dir: &Path,
) -> (
    std::collections::HashMap<String, f32>,
    Option<f32>,
    Option<usize>,
    Option<f32>,
) {
    let mut empty = (std::collections::HashMap::new(), None, None, None);
    let text = match std::fs::read_to_string(dir.join("thresholds.json")) {
        Ok(t) => t,
        Err(_) => return empty,
    };
    let cfg: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return empty,
    };
    let per_tool = cfg["per_tool"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_f64().map(|x| (k.clone(), x as f32)))
                .collect()
        })
        .unwrap_or_default();
    empty.0 = per_tool;
    empty.1 = cfg["min_score"].as_f64().map(|x| x as f32);
    empty.2 = cfg["max_tools"].as_u64().map(|x| x as usize);
    empty.3 = cfg["temperature"].as_f64().map(|x| x as f32);
    empty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_bundle_is_not_an_error() {
        let dir = std::env::temp_dir().join("rijo-router-missing-test");
        let loaded = RouterSession::load_from(&dir).expect("load must not fail");
        assert!(loaded.is_none(), "missing bundle must load as None");
    }

    /// The invariant of §Y, checked in every build configuration: a router
    /// that cannot be there answers `None`, never an error. With the `onnx`
    /// feature that means "no bundle in this directory"; without it, `load`
    /// answers `None` for any directory at all. Both are the same guarantee
    /// seen from two sides — which is the point of the test: it has to hold
    /// whether or not the ONNX runtime was available at build time.
    #[test]
    fn an_absent_router_is_never_an_error() {
        let dir = std::env::temp_dir().join(format!("rijo-router-absent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");

        let loaded = RouterSession::load_from(&dir).expect("load must not fail");
        assert!(
            loaded.is_none(),
            "an empty directory must load as None in every configuration"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sigmoid_and_softmax_are_normalized() {
        let s = sigmoid(&[-2.0, 0.0, 2.0]);
        assert!((s[0] - 0.1192).abs() < 0.001);
        assert!((s[1] - 0.5).abs() < 0.001);
        let p = softmax(&[1.0, 1.0, 2.0]);
        let sum: f32 = p.iter().sum();
        assert!((sum - 1.0).abs() < 1e-5, "softmax must sum to 1, got {sum}");
        assert!(p[2] > p[0]);
    }

    #[test]
    fn config_defaults_when_absent() {
        let dir = std::env::temp_dir().join("rijo-router-cfg-missing");
        assert_eq!(read_config(&dir), (0.25, 8, 256));
        let (per_tool, min, max, temp) = read_thresholds(&dir);
        assert!(per_tool.is_empty());
        assert!(min.is_none() && max.is_none() && temp.is_none());
    }

    #[test]
    fn thresholds_file_is_read_when_present() {
        let dir = std::env::temp_dir().join(format!("rijo-router-thr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        std::fs::write(
            dir.join("thresholds.json"),
            r#"{"per_tool":{"read_file":0.42},"max_tools":7,"min_score":0.1,"temperature":0.6}"#,
        )
        .expect("write");
        let (per_tool, min, max, temp) = read_thresholds(&dir);
        assert_eq!(per_tool.get("read_file"), Some(&0.42));
        assert_eq!(min, Some(0.1));
        assert_eq!(max, Some(7));
        assert_eq!(temp, Some(0.6));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real ONNX bundle check. Opt-in: point RIJO_ROUTER_TEST_BUNDLE at an
    /// exported bundle (scripts/router/export_onnx.py output) and run with
    /// `--ignored`. Without the env var the test is a no-op.
    #[test]
    #[ignore = "needs an exported ONNX bundle"]
    fn scores_with_real_bundle() {
        let dir = std::env::var("RIJO_ROUTER_TEST_BUNDLE").expect("RIJO_ROUTER_TEST_BUNDLE");
        let mut session = RouterSession::load_from(Path::new(&dir))
            .expect("load must not fail")
            .expect("bundle must be present");
        // The bundle must cover every registered tool — a mismatch is label drift
        // and would silently drop tools from routing (compare to the live registry
        // so adding a tool without retraining fails here instead of in production).
        let expected = crate::tools::Registry::build_default().get_names().len();
        assert_eq!(
            session.tool_names().len(),
            expected,
            "the bundle must cover every registered tool"
        );
        session.set_max_tools(6);
        let out = session.score("Zeig mir die ersten 50 Zeilen von Cargo.toml");
        assert!(out.tools.len() <= 6, "config max_tools override");
        assert!(out.n_proposed >= out.tools.len());
        assert!(out.toolset.is_some(), "toolset head must return a class");
        assert!(!out.guard.is_empty(), "guard head must return classes");
        assert!(out.compact.is_some(), "compact head must return a class");
        let mut times: Vec<u64> = Vec::new();
        for _ in 0..10 {
            let t = std::time::Instant::now();
            let repeat = session.score("Lies die Konfigurationsdatei und fasse sie zusammen");
            times.push(t.elapsed().as_millis() as u64);
            assert!(repeat.router_ms <= 2000);
        }
        times.sort_unstable();
        println!(
            "cold_ms={} median_ms={} max_ms={} proposed={} tools={:?}",
            out.router_ms,
            times[times.len() / 2],
            times[times.len() - 1],
            out.n_proposed,
            out.tools
        );
    }

    /// One binary has to work in three layouts: dev/test (target/), the Tauri
    /// bundle (resources/) and the CLI installer (<rijo_home>/bin).
    #[test]
    fn the_runtime_dll_is_looked_for_beside_the_binary_then_in_resources() {
        let exe = std::path::Path::new("C:\\Apps\\Rijo Code");
        let candidates = ort_candidates(exe);
        assert_eq!(candidates[0], exe.join("onnxruntime.dll"));
        assert_eq!(
            candidates[2],
            exe.join("resources").join("onnxruntime.dll"),
            "the bundle puts resources next to the exe"
        );
        assert!(
            candidates.last().unwrap().ends_with("bin/onnxruntime.dll"),
            "{:?}",
            candidates
        );
    }
}
