//! Todo tool — one persistent task list per workspace.
//!
//! Path: `<data_dir>/todos/<workspace-key>.json`. The list used to live in a
//! single global `todos.json`, which was harmless while Rijo only served one
//! project. The desktop app serves several: a stale plan from the Rijo repo then
//! showed up in a static-site workspace and sent the model auditing "Crates" in
//! a site with no Cargo.toml (measured 2026-10-17).

use crate::tools::{tool_error, JsonSchema, Tool, ToolMeta, ToolResult};
use crate::AppState;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct TodoTool;

/// Stable per-workspace filename key (FNV-1a over the canonical path). Not a
/// security primitive: the file only holds this workspace's task list.
fn workspace_key(workspace: &Path) -> String {
    let canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let mut hash = 0xcbf29ce484222325u64;
    for byte in canonical.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Path of one workspace's todo list. `data_dir` stays shared; only the list is
/// scoped, so two open projects cannot overwrite each other's plan.
pub fn path_for(data_dir: &Path, workspace: &Path) -> PathBuf {
    data_dir
        .join("todos")
        .join(format!("{}.json", workspace_key(workspace)))
}

fn todo_path(state: &AppState) -> PathBuf {
    path_for(&state.config.data_dir, &state.config.workspace)
}

/// (done, total) for the workspace, for status displays.
pub fn counts(data_dir: &Path, workspace: &Path) -> (usize, usize) {
    let Ok(text) = std::fs::read_to_string(path_for(data_dir, workspace)) else {
        return (0, 0);
    };
    let Ok(items) = serde_json::from_str::<Vec<Value>>(&text) else {
        return (0, 0);
    };
    let done = items
        .iter()
        .filter(|item| item.get("status").and_then(Value::as_str) == Some("completed"))
        .count();
    (done, items.len())
}

fn read_todos(state: &AppState) -> Vec<Value> {
    std::fs::read_to_string(todo_path(state))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

fn write_todos(state: &AppState, todos: &[Value]) -> Result<(), String> {
    let path = todo_path(state);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let data = serde_json::to_string_pretty(todos).map_err(|e| e.to_string())?;
    std::fs::write(&path, data).map_err(|e| e.to_string())
}

/// Render a plan block from a todo array. Pure, so the format is unit-tested.
///
/// The model must SEE its own list: it only lives in a file, and a model that has to
/// remember reading it restarts at step 1 (real report — the strip stayed on
/// "Markup lesen" while it was writing `main.js`). Empty array -> empty block.
pub fn render_plan(todos: &[Value]) -> String {
    if todos.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n## Current plan (your todo_list — keep it up to date)\n");
    for t in todos {
        let mark = match t["status"].as_str().unwrap_or("pending") {
            "completed" => "x",
            "in_progress" => ">",
            "cancelled" => "-",
            _ => " ",
        };
        out.push_str("- [");
        out.push_str(mark);
        out.push_str("] ");
        out.push_str(t["content"].as_str().unwrap_or(""));
        out.push('\n');
    }
    out.push_str(
        "Mark finished steps `completed`, keep exactly ONE `in_progress`, and send the \
         FULL list with `todo_list` — an out-of-date list is worse than none.\n",
    );
    out
}

/// The plan block for the current turn's user message ("" when there is no list).
pub fn plan_block(state: &AppState) -> String {
    render_plan(&read_todos(state))
}

impl Tool for TodoTool {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "todo_list",
            description: "Task list for this workspace. Send todos to set, omit to read.",
            emoji: "✅",
            toolset: "core",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &TODO_SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        // Read-only mode
        let todos_provided = args.get("todos").and_then(|v| v.as_array());
        let Some(todos_provided) = todos_provided else {
            return json!({ "todos": read_todos(&state) }).to_string();
        };

        let merge = args.get("merge").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut current = if merge {
            read_todos(&state)
        } else {
            Vec::new()
        };

        for (idx, item) in todos_provided.iter().enumerate() {
            // id is optional: if missing we derive a stable id so that
            // a call without an id does not fail completely (Hermes callers
            // send it, others do not).
            let id_owned = item
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("todo-{}", idx + 1));
            let id: &str = &id_owned;
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending");
            let content = item.get("content").and_then(|v| v.as_str()).unwrap_or("");

            match status {
                "cancelled" => {
                    current.retain(|t| t["id"].as_str() != Some(id));
                }
                _ => {
                    let entry = json!({"id": id, "content": content, "status": status});
                    if let Some(pos) = current.iter().position(|t| t["id"].as_str() == Some(id)) {
                        current[pos] = entry;
                    } else {
                        current.push(entry);
                    }
                }
            }
        }

        if let Err(e) = write_todos(&state, &current) {
            return tool_error(format!("Cannot persist todos: {e}"));
        }
        // Refresh the TUI strip. Best-effort: without a TUI (headless, MCP) this is
        // a no-op, the file above is the source of truth.
        let sender = state
            .ui_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(tx) = sender {
            let _ = tx.send(crate::ui_events::UiEvent::TodosChanged);
        }
        json!({ "todos": current }).to_string()
    }
}

lazy_static::lazy_static! {
    static ref TODO_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "todos": {
                "type": "array",
                "description": "Todo items to write",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "content": { "type": "string" },
                        "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"] }
                    },
                    "required": ["id"]
                }
            },
            "merge": { "type": "boolean", "description": "Merge by id instead of replacing" }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The plan block the model sees: statuses as marks; no list -> no block.
    #[test]
    fn the_plan_block_renders_statuses_and_is_empty_without_a_list() {
        assert_eq!(render_plan(&[]), "", "no list -> no block");

        let todos = vec![
            json!({"id": "1", "content": "Recherche", "status": "completed"}),
            json!({"id": "2", "content": "main.js schreiben", "status": "in_progress"}),
            json!({"id": "3", "content": "Verify", "status": "pending"}),
            json!({"id": "4", "content": "Alt", "status": "cancelled"}),
        ];
        let block = render_plan(&todos);
        assert!(block.contains("## Current plan"), "{block}");
        assert!(block.contains("- [x] Recherche"), "{block}");
        assert!(block.contains("- [>] main.js schreiben"), "{block}");
        assert!(block.contains("- [ ] Verify"), "{block}");
        assert!(block.contains("- [-] Alt"), "{block}");
        assert!(
            block.contains("keep exactly ONE `in_progress`"),
            "the rule travels with the block: {block}"
        );
    }

    use std::path::Path;

    #[test]
    fn each_workspace_gets_its_own_todo_file() {
        let data = Path::new("C:/data");
        let a = path_for(data, Path::new("C:/work/a"));
        let b = path_for(data, Path::new("C:/work/b"));
        assert_ne!(a, b, "two projects must not share a list");
        assert!(a.starts_with(data.join("todos")), "{}", a.display());
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("json"));
    }

    /// Regression for the 2026-10-17 run: a stale plan from the Rijo repo made
    /// the model audit "Crates" in a static-site workspace.
    #[test]
    fn counts_read_only_the_given_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = dir.path().join("data");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).expect("a");
        std::fs::create_dir_all(&b).expect("b");

        let path = path_for(&data, &a);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(
            &path,
            r#"[{"id":"1","content":"x","status":"completed"},{"id":"2","content":"y","status":"pending"}]"#,
        )
        .expect("write");

        assert_eq!(counts(&data, &a), (1, 2));
        assert_eq!(counts(&data, &b), (0, 0), "workspace b must start clean");
    }
}
