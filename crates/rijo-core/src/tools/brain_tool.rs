//! `brain_search` — ask the indexed workspace directly.
//!
//! Why this tool exists: the brain indexer's docstring promised "the LLM can use
//! brain_query instead of read_file", but no such tool was ever registered. The
//! brain only pushed three file:line pointers into the system prompt, so the
//! model could neither ask for more, nor for content, nor narrow the query. Real
//! runs showed the cost: 7-8 repeated read_file calls on the same file.
//!
//! The tool returns what the model needs to decide whether to open a file at
//! all: pointer, score and a short excerpt. Output is bounded (see EXCERPT_CHARS
//! and MAX_HITS) because every character lands in the context window.

use crate::brain::db::{SearchMode, SearchQuery};
use crate::tools::{tool_error, JsonSchema, Tool, ToolMeta, ToolResult};
use crate::AppState;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct BrainSearch;

/// Excerpt per hit. Enough to recognise the code, not enough to replace reading
/// the file when the details matter.
const EXCERPT_CHARS: usize = 500;
/// Hard cap on hits, independent of what the model asks for.
const MAX_HITS: usize = 20;
/// Default when the model does not say.
const DEFAULT_HITS: usize = 5;

lazy_static::lazy_static! {
    static ref BRAIN_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "What to look for, in natural language or keywords (e.g. 'where is the tool registry built')."
            },
            "top_k": {
                "type": "integer",
                "description": "How many hits (default 5, max 20)."
            },
            "kind": {
                "type": "string",
                "description": "Only this node kind, e.g. 'code'."
            },
            "path_prefix": {
                "type": "string",
                "description": "Only hits whose source path starts with this (e.g. 'crates/rijo-core/src/tools/')."
            },
            "all_workspaces": {
                "type": "boolean",
                "description": "Search every indexed project, not just the current workspace (default false)."
            }
        },
        "required": ["query"]
    }));
}

impl Tool for BrainSearch {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "brain_search",
            // Deliberately talks about the alternative: the point of the tool is
            // to replace blind read_file calls, not to add another toy.
            description: "Search the indexed workspace (code chunks) by meaning or keyword; returns file pointers, scores and excerpts. Use it before opening a file or grepping blindly.",
            emoji: "🧠",
            toolset: "brain",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &BRAIN_SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        // One switch for the whole brain: with `memory.enabled: false` neither the
        // prompt context nor this tool is available, so an A/B run measures the
        // feature and not a half-disabled variant.
        if !state.config.memory.enabled {
            return json!({
                "mode": "off",
                "hits": [],
                "note": "The brain is disabled (memory.enabled: false). Use search_files/read_file instead.",
            })
            .to_string();
        }
        let Some(query_text) = args.get("query").and_then(|v| v.as_str()) else {
            return tool_error("query is required");
        };
        if query_text.trim().is_empty() {
            return tool_error("query must not be empty");
        }
        let top_k = args
            .get("top_k")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, MAX_HITS))
            .unwrap_or(DEFAULT_HITS);
        let kind = args.get("kind").and_then(|v| v.as_str());
        let path_prefix = args.get("path_prefix").and_then(|v| v.as_str());
        let all_workspaces = args
            .get("all_workspaces")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let workspace_key = {
            let ws = state.config.workspace.clone();
            ws.canonicalize()
                .unwrap_or(ws)
                .to_string_lossy()
                .to_string()
        };

        // Embedding may be unavailable (indexer holds the embedder, or the model
        // file is missing) — then the search falls back to full text and says so.
        let embedding: Option<Vec<f32>> = state
            .embedder
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|e| e.embed_text(query_text).ok()));

        let q = SearchQuery {
            text: query_text,
            embedding: embedding.as_deref(),
            kind,
            workspace: if all_workspaces {
                None
            } else {
                Some(workspace_key.as_str())
            },
            // Over-fetch so a path_prefix filter still leaves enough hits.
            top_k: if path_prefix.is_some() {
                MAX_HITS
            } else {
                top_k
            },
            min_score: state.config.memory.min_score,
        };

        let result = match state.brain.search(&q) {
            Ok(r) => r,
            Err(e) => return tool_error(format!("brain search failed: {e}")),
        };
        let mode = match result.mode {
            SearchMode::Vector => "vector",
            SearchMode::FullText => "fulltext",
            SearchMode::Hybrid => "hybrid",
        };

        let mut hits: Vec<Value> = Vec::new();
        for hit in result.hits.iter() {
            if let Some(prefix) = path_prefix {
                if !hit.node.source.starts_with(prefix) {
                    continue;
                }
            }
            let excerpt: String = hit.node.content.chars().take(EXCERPT_CHARS).collect();
            hits.push(json!({
                "source": hit.node.source,
                "label": hit.node.label,
                "kind": hit.node.kind,
                "score": (hit.score * 1000.0).round() / 1000.0,
                "tokens_est": hit.node.token_cost,
                "excerpt": excerpt,
            }));
            if hits.len() >= top_k {
                break;
            }
        }

        if hits.is_empty() {
            // Actionable emptiness: the model should know whether the workspace
            // has no index at all (one-shot runs skip indexing) or the query just
            // did not match.
            let indexed = state.brain.count_nodes(Some(&workspace_key)).unwrap_or(0);
            return json!({
                "mode": mode,
                "workspace": workspace_key,
                "hits": [],
                "indexed_nodes_in_workspace": indexed,
                "note": if indexed == 0 {
                    "Nothing indexed for this workspace yet (indexing runs at interactive start; one-shot runs skip it). Fall back to search_files/read_file."
                } else {
                    "No hit above the score floor. Rephrase with identifiers from the code, or use path_prefix, or fall back to search_files."
                },
            })
            .to_string();
        }

        // Count the hits the model actually received: the only honest feedback
        // signal we have (see BrainDb::bump_access).
        let used: Vec<String> = result
            .hits
            .iter()
            .filter(|h| {
                hits.iter()
                    .any(|v| v["source"] == h.node.source && v["label"] == h.node.label)
            })
            .map(|h| h.node.id.clone())
            .collect();
        let _ = state.brain.bump_access(&used);

        json!({
            "mode": mode,
            "workspace": workspace_key,
            "hits": hits,
            "note": if mode == "fulltext" {
                "Full-text only: the vector index was busy, so results are keyword-based and unsorted by meaning."
            } else if mode == "hybrid" {
                "Vector and keyword search fused by rank (a hit both paths like ranks higher). Pointers + excerpts; open the file only if you need more than the excerpt."
            } else {
                "Pointers + excerpts; open the file only if you need more than the excerpt."
            },
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::db::{BrainDb, Node};

    fn state_with(workspace: &std::path::Path, nodes: &[(&str, &str)]) -> Arc<AppState> {
        let mut cfg = crate::config::Config::default();
        // tempfile auf Windows liefert verbatim-Pfade (`\\?\C:\Users\...`).
        // brain_search vergleicht den Workspace-String mit dem gespeicherten
        // Wert — ohne canonicalize finden sich die Nodes nicht, weil der
        // Prefix fehlt. Der kanonische Pfad ist kurz genug und ohne Prefix.
        let canonical = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        cfg.workspace = canonical.clone();
        cfg.memory.min_score = 0.0; // keep the test about the tool, not the floor
        let brain = Arc::new(BrainDb::open(&workspace.join("brain.db"), 4).expect("brain"));
        for (name, content) in nodes {
            let node = Node::new(
                format!("code:{name}"),
                format!("{name} — fn example"),
                content.to_string(),
                "code",
            )
            .with_source(name)
            .with_workspace(canonical.to_string_lossy().as_ref());
            // Kein Embedding: die Tests pruefen den Werkzeugpfad ueber den
            // Workspace-Schluessel und die Volltextsuche, nicht die
            // semantische Aehnlichkeit. `None` haelt `mode` bei "fulltext"
            // und macht das Ergebnis deterministisch.
            brain.upsert_node(&node, None).expect("upsert");
        }
        Arc::new(AppState {
            brain,
            embedder: tokio::sync::Mutex::new(None),
            config: cfg,
            session_id: crate::AppState::new_session_id(),
            tools: Arc::new(crate::tools::Registry::build_default()),
            ui_tx: std::sync::Mutex::new(None),
            files_read: std::sync::Mutex::new(std::collections::HashSet::new()),
            cancel: std::sync::atomic::AtomicBool::new(false),
            router: std::sync::Mutex::new(None),
        })
    }

    #[test]
    fn returns_hits_with_excerpt_and_reports_full_text_mode() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(
            dir.path(),
            &[("registry.rs", "fn build_default registers every tool here")],
        );
        let out = BrainSearch
            .call(json!({ "query": "registers" }), state)
            .to_string();
        let v: Value = serde_json::from_str(&out).unwrap();
        // Ohne Embedder laeuft nur der lexikalische Pfad — genau das soll
        // dieser Test festhalten (der Vektor-Abruf hat eigene Tests).
        assert_eq!(
            v["mode"], "fulltext",
            "no embedder in the test -> full text: {out}"
        );
        assert_eq!(v["hits"].as_array().unwrap().len(), 1, "{out}");
        assert_eq!(v["hits"][0]["source"], "registry.rs");
        assert!(v["hits"][0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("build_default"));
        assert!(v["note"].as_str().unwrap().contains("Full-text only"));
    }

    #[test]
    fn empty_result_explains_what_to_do_instead() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(dir.path(), &[("a.rs", "fn a() {}")]);
        let out = BrainSearch
            .call(json!({ "query": "xyzzy-no-match" }), state)
            .to_string();
        let v: Value = serde_json::from_str(&out).unwrap();
        // Kein Embedder im Test, kein Volltext-Treffer fuer "xyzzy-no-match":
        // das Ergebnis ist leer und die Antwort sagt, was stattdessen zu tun ist.
        assert!(v["hits"].as_array().unwrap().is_empty(), "{out}");
        assert_eq!(v["indexed_nodes_in_workspace"], 1);
        assert!(
            v["note"].as_str().unwrap().contains("search_files"),
            "{out}"
        );
    }

    #[test]
    fn path_prefix_narrows_the_hits() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(
            dir.path(),
            &[
                ("crates/core/registry.rs", "registers tools"),
                ("docs/registry.md", "registers tools in prose"),
            ],
        );
        let out = BrainSearch
            .call(
                json!({ "query": "registers", "path_prefix": "crates/" }),
                state,
            )
            .to_string();
        let v: Value = serde_json::from_str(&out).unwrap();
        let hits = v["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{out}");
        assert_eq!(hits[0]["source"], "crates/core/registry.rs");
    }

    #[test]
    fn missing_query_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(dir.path(), &[]);
        let out = BrainSearch.call(json!({}), state).to_string();
        assert!(out.contains("query is required"), "{out}");
    }

    #[test]
    fn another_workspace_is_not_searched_unless_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let state = state_with(dir.path(), &[("mine.rs", "only mine registers")]);
        // Seed a node that belongs to a different workspace, in the same DB.
        let foreign = Node::new(
            "code:foreign".into(),
            "foreign — fn".into(),
            "only foreign registers".into(),
            "code",
        )
        .with_source("foreign.rs")
        .with_workspace(other.path().to_string_lossy().as_ref());
        state.brain.upsert_node(&foreign, None).unwrap();

        let scoped = BrainSearch
            .call(json!({ "query": "registers" }), state.clone())
            .to_string();
        assert!(scoped.contains("mine.rs"), "{scoped}");
        assert!(
            !scoped.contains("foreign.rs"),
            "foreign workspace leaked: {scoped}"
        );

        let all = BrainSearch
            .call(
                json!({ "query": "registers", "all_workspaces": true }),
                state,
            )
            .to_string();
        assert!(
            all.contains("foreign.rs"),
            "all_workspaces must cross projects: {all}"
        );
    }

    #[test]
    fn disabled_brain_answers_with_a_pointer_to_the_alternatives() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state_with(dir.path(), &[("a.rs", "fn a() {}")]);
        Arc::get_mut(&mut state).unwrap().config.memory.enabled = false;
        let out = BrainSearch.call(json!({ "query": "a" }), state).to_string();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mode"], "off", "{out}");
        assert!(v["hits"].as_array().unwrap().is_empty());
        assert!(
            v["note"].as_str().unwrap().contains("search_files"),
            "{out}"
        );
    }
}
