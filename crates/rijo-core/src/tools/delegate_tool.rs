//! Delegate tool — spawn isolated subagent processes (rijo --child <context-file>)
//!
//! Each child runs as its own OS process with its own session id. The parent
//! passes the full task brief via a context file; the child's final response
//! is written to a result file which the parent polls once per wait cycle.
//! Real process-level isolation — no shared state with the parent.

use crate::tools::{tool_error, AsyncTool, JsonSchema, ToolMeta, ToolResult};
use crate::AppState;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct DelegateTask;

const CHILD_TIMEOUT_SECS: u64 = 600;

#[derive(serde::Deserialize)]
struct SubTask {
    goal: String,
    #[serde(default)]
    context: String,
}

#[async_trait]
impl AsyncTool for DelegateTask {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "delegate_task",
            description: "Spawn subagents in isolation; each returns a summary.",
            emoji: "👤",
            toolset: "core",
            is_async: true,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &DELEGATE_SCHEMA
    }

    async fn call_async(&self, args: Value, _state: Arc<AppState>) -> ToolResult {
        let tasks = match args.get("tasks").and_then(|v| v.as_array()) {
            Some(t) => t,
            None => return tool_error("tasks array is required"),
        };

        let mut handles = Vec::new();
        for t in tasks {
            let sub: SubTask = match serde_json::from_value(t.clone()) {
                Ok(s) => s,
                Err(e) => return tool_error(format!("bad task: {e}")),
            };

            let brief = format!(
                "{}\n\nBackground:\n{}",
                sub.goal,
                if sub.context.is_empty() {
                    "(none)".into()
                } else {
                    sub.context.clone()
                }
            );

            // Write brief to a temp file the child will read
            let id = uuid::Uuid::new_v4().simple().to_string();
            let brief_path = std::env::temp_dir().join(format!("rijo_del_{id}.txt"));
            let result_path = std::env::temp_dir().join(format!("rijo_res_{id}.txt"));
            let _ = std::fs::write(&brief_path, &brief);
            let _ = std::fs::remove_file(&result_path);

            let exe = std::env::current_exe().unwrap_or_else(|_| "rijo".into());
            let child = tokio::process::Command::new(exe)
                .arg("--child")
                .arg(&brief_path)
                .arg(&result_path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();

            match child {
                Ok(c) => handles.push((c, result_path)),
                Err(e) => return tool_error(format!("spawn failed: {e}")),
            }
        }

        // Wait for all children with a combined timeout
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(CHILD_TIMEOUT_SECS);
        let mut results = Vec::new();
        for (mut child, result_path) in handles {
            match tokio::time::timeout_at(deadline, child.wait()).await {
                Ok(Ok(status)) => {
                    let text = std::fs::read_to_string(&result_path).unwrap_or_default();
                    results.push(json!({
                        "exit_code": status.code().unwrap_or(-1),
                        "result": if text.is_empty() { "(no result written)".to_string() } else { text },
                    }));
                    let _ = std::fs::remove_file(&result_path);
                }
                Ok(Err(e)) => results.push(json!({"error": format!("child error: {e}")})),
                Err(_) => results.push(json!({"error": "subagent timed out"})),
            }
        }

        json!({ "results": results, "count": results.len() }).to_string()
    }
}

/// Child-mode handler — reads a brief file, runs the agent loop once, writes the result.
/// Invoked via `rijo --child <in> <out>`.
pub async fn run_child(brief_path: &std::path::Path, result_path: &std::path::Path) {
    // LOAD the config, do not default it: with Config::default() the subagent got
    // the default provider (OpenAI) without a key — every delegated run ended
    // with "401 You didn't provide an API key", regardless of the
    // configured environment of the parent process.
    let config = crate::config::Config::load(None);
    let executor = crate::executor::Executor::new(config);
    let brief = std::fs::read_to_string(brief_path).unwrap_or_default();
    let mut history = Vec::new();

    let outcome = executor.run_turn(&brief, &mut history).await;
    let body = match outcome {
        Ok(text) => text,
        Err(e) => format!("child failed: {e}"),
    };
    if let Ok(mut f) = std::fs::File::create(result_path) {
        use std::io::Write as _;
        let _ = f.write_all(body.as_bytes());
    }
}

lazy_static::lazy_static! {
    static ref DELEGATE_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "tasks": {
                "type": "array",
                "maxItems": 10,
                "items": {
                    "type": "object",
                    "properties": {
                        "goal": { "type": "string", "description": "Goal of this subagent" },
                        "context": { "type": "string", "description": "Context for the child (no parent state)" }
                    },
                    "required": ["goal"]
                }
            }
        },
        "required": ["tasks"]
    }));
}
