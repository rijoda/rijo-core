//! Tool router — which tool schemas this turn actually needs.
//!
//! Why: the tool schemas are ~10k characters, the largest fixed part of the
//! prompt per turn (measured, see docs/GOAL-100). A small local encoder
//! (DistilBERT multilingual, ONNX via `ort`) scores the prompt; the executor
//! sends only the tools above their calibrated threshold. The full tool set
//! is always the fallback — a missing or broken model must never change
//! behaviour, only cost.
//!
//! The model lives outside the binary in `router_model_dir()` (trained
//! offline with `scripts/router/`, exported as ONNX + tokenizer + labels).
//!
//! M0 (this milestone): model location and the label dump — the labels pin
//! the neuron order, and `--dump-labels` keeps the Rust registry and the
//! Python dataset exporter from silently disagreeing about it. The session
//! itself (`RouterSession`, ONNX + tokenizer) lands with the runtime module.

use serde_json::{json, Value};
use std::path::PathBuf;

use crate::tools::Registry;

pub mod session;

pub use session::{RouterOutput, RouterSession};

/// Stable location of the router model bundle: `<models>/router/<version>`.
///
/// Same precedence as the embedding cache: `RIJO_MODEL_CACHE` (tests, CI,
/// custom installs) as the models root, otherwise `~/.rijo/models`. The
/// directory is created on demand; the bundle itself is placed there by
/// `scripts/router/export_onnx.py`.
pub fn router_model_dir(version: &str) -> PathBuf {
    let root = std::env::var("RIJO_MODEL_CACHE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::rijo_home().join("models"));
    root.join("router").join(version)
}

/// Label tables from the registry: tool name → toolset, in the exact order
/// the model's neurons are trained on (alphabetical — the same order
/// `Registry::get_names` sends the schemas in).
///
/// Writing this from the registry instead of hand-maintaining it is the
/// point: a renamed or added tool shows up as a mismatch in the dataset
/// exporter instead of training against a stale list.
pub fn labels_from_registry(registry: &Registry) -> Value {
    let tools: Vec<Value> = registry
        .get_names()
        .iter()
        .map(|name| {
            let toolset = registry
                .get_meta(name)
                .map(|m| m.toolset.to_string())
                .unwrap_or_default();
            json!({ "name": name, "toolset": toolset })
        })
        .collect();
    let mut toolsets: Vec<String> = tools
        .iter()
        .filter_map(|t| t["toolset"].as_str().map(String::from))
        .filter(|s| !s.is_empty())
        .collect();
    toolsets.sort_unstable();
    toolsets.dedup();
    json!({
        "kind": "labels_registry",
        "ts": chrono::Utc::now().to_rfc3339(),
        // The order IS the neuron order — the exporter must not sort on its own.
        "order": "alphabetical (Registry::get_names)",
        "count": tools.len(),
        "tools": tools,
        "toolsets": toolsets,
    })
}

/// Writes the label tables to `<data_dir>/router/labels_registry.json` and
/// returns the path (for `--dump-labels` and the dataset exporter).
pub fn dump_labels(registry: &Registry, data_dir: &std::path::Path) -> Result<PathBuf, String> {
    let dir = data_dir.join("router");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join("labels_registry.json");
    let labels = labels_from_registry(registry);
    let text = serde_json::to_string_pretty(&labels).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_cover_every_registered_tool() {
        let registry = Registry::build_default();
        let labels = labels_from_registry(&registry);
        let names: Vec<&str> = registry.get_names();
        let listed: Vec<&str> = labels["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(listed, names, "label order must be the registry order");
        assert_eq!(labels["count"].as_u64().unwrap(), names.len() as u64);
        assert!(
            labels["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|t| { !t["toolset"].as_str().unwrap_or("").is_empty() }),
            "every tool needs a toolset"
        );
    }

    #[test]
    fn toolsets_are_sorted_and_unique() {
        let registry = Registry::build_default();
        let labels = labels_from_registry(&registry);
        let toolsets: Vec<&str> = labels["toolsets"]
            .as_array()
            .expect("toolsets array")
            .iter()
            .filter_map(|t| t.as_str())
            .collect();
        let mut sorted = toolsets.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(toolsets, sorted, "toolsets must be sorted and unique");
    }
}
