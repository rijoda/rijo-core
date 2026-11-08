//! Memory tool — persistent, curated memory across sessions
//!
//! Two stores: `memory` (agent's notes) and `user` (user profile).
//! Stored as delimited entries in ~/.rijo/memories/MEMORY.md and USER.md.

use crate::tools::{tool_error, JsonSchema, Tool, ToolMeta, ToolResult};
use crate::AppState;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

pub struct MemoryTool;

const ENTRY_DELIMITER: &str = "\n§\n";
const MAX_CHARS: usize = 2200;

fn memory_file(target: &str, state: &AppState) -> PathBuf {
    let dir = state.config.data_dir.join("memories");
    let name = match target {
        "user" => "USER.md",
        _ => "MEMORY.md",
    };
    dir.join(name)
}

fn read_entries(target: &str, state: &AppState) -> Vec<String> {
    let path = memory_file(target, state);
    if !path.exists() {
        return Vec::new();
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    content
        .split(ENTRY_DELIMITER)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Read one of the two user-owned memory stores without modifying it.
pub fn list_entries(state: &AppState, target: &str) -> Vec<String> {
    match target {
        "memory" | "user" => read_entries(target, state),
        _ => Vec::new(),
    }
}

fn write_entries(target: &str, entries: &[String], state: &AppState) -> Result<(), String> {
    let path = memory_file(target, state);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = entries.join(ENTRY_DELIMITER);
    std::fs::write(&path, &content).map_err(|e| e.to_string())
}

impl Tool for MemoryTool {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "memory",
            description: "Save durable facts across sessions. Stores: 'memory', 'user'.",
            emoji: "🧠",
            toolset: "core",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &MEMORY_SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("add");
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("memory");

        let content = args.get("content").and_then(|v| v.as_str());
        let new_text = args.get("new_text").and_then(|v| v.as_str());
        let content = content.or(new_text);

        let old_text = args.get("old_text").and_then(|v| v.as_str());

        let operations = args.get("operations").and_then(|v| v.as_array());

        // Handle batch operations
        if let Some(ops) = operations {
            let mut entries = read_entries(target, &state);
            for op in ops {
                let op_action = op.get("action").and_then(|v| v.as_str()).unwrap_or("");
                let op_content = op
                    .get("content")
                    .or_else(|| op.get("new_text"))
                    .and_then(|v| v.as_str());
                let op_old = op.get("old_text").and_then(|v| v.as_str());

                match op_action {
                    "add" => {
                        if let Some(c) = op_content {
                            entries.push(c.to_string());
                        }
                    }
                    "remove" => {
                        if let Some(old) = op_old {
                            entries.retain(|e| !e.contains(old));
                        }
                    }
                    "replace" => {
                        if let (Some(old), Some(new)) = (op_old, op_content) {
                            if let Some(pos) = entries.iter().position(|e| e.contains(old)) {
                                entries[pos] = new.to_string();
                            }
                        }
                    }
                    _ => {}
                }
            }

            // Check char limit
            let total: usize = entries.iter().map(|e| e.len() + 2).sum();
            if total > MAX_CHARS {
                return json!({
                    "error": format!("Memory at capacity ({} chars). Remove or shorten entries before adding.", total),
                    "entries": entries,
                    "current_chars": total,
                    "max_chars": MAX_CHARS
                }).to_string();
            }

            return match write_entries(target, &entries, &state) {
                Ok(()) => json!({
                    "success": true,
                    "entry_count": entries.len(),
                    "current_chars": total,
                    "max_chars": MAX_CHARS
                })
                .to_string(),
                Err(e) => tool_error(format!("Failed to write memory: {e}")),
            };
        }

        // Single operation
        let mut entries = read_entries(target, &state);

        match action {
            "add" => {
                if let Some(c) = content {
                    entries.push(c.to_string());
                }
            }
            "remove" => {
                if let Some(old) = old_text {
                    entries.retain(|e| !e.contains(old));
                }
            }
            "replace" => {
                if let (Some(old), Some(new)) = (old_text, content) {
                    if let Some(pos) = entries.iter().position(|e| e.contains(old)) {
                        entries[pos] = new.to_string();
                    } else {
                        return tool_error(format!("Could not find entry containing: {old}"));
                    }
                }
            }
            _ => return tool_error(format!("Unknown action: {action}")),
        }

        let total: usize = entries.iter().map(|e| e.len() + 2).sum();
        if total > MAX_CHARS {
            return tool_error(format!(
                "Memory at capacity ({total} chars). Use batch operations to remove or shorten entries."
            ));
        }

        match write_entries(target, &entries, &state) {
            Ok(()) => json!({
                "success": true,
                "entry_count": entries.len(),
                "current_chars": total,
                "max_chars": MAX_CHARS
            })
            .to_string(),
            Err(e) => tool_error(format!("Failed to write memory: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_list_entries_returns_separate_stores() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.data_dir = dir.path().to_path_buf();
        let state = crate::AppState {
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
        };
        let memory_dir = state.config.data_dir.join("memories");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join("MEMORY.md"), "fact one\n§\nfact two").unwrap();
        std::fs::write(memory_dir.join("USER.md"), "user preference").unwrap();

        assert_eq!(list_entries(&state, "memory"), vec!["fact one", "fact two"]);
        assert_eq!(list_entries(&state, "user"), vec!["user preference"]);
        assert!(list_entries(&state, "unexpected").is_empty());
    }
}

lazy_static::lazy_static! {
    static ref MEMORY_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["add", "replace", "remove"],
                "description": "What to do"
            },
            "target": {
                "type": "string",
                "enum": ["memory", "user"],
                "description": "'memory' or 'user'"
            },
            "content": {
                "type": "string",
                "description": "Content for add/replace"
            },
            "old_text": {
                "type": "string",
                "description": "Substring for replace/remove"
            },
            "operations": {
                "type": "array",
                "description": "Batch operations",
                "items": {
                    "type": "object",
                    "properties": {
                        "action": { "type": "string" },
                        "content": { "type": "string" },
                        "old_text": { "type": "string" }
                    }
                }
            }
        },
        "required": ["target"]
    }));
}
