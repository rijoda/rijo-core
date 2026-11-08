//! process_manage — observe and control background processes.
//!
//! Works on the registry in `crate::process` (where processes from
//! `terminal(background: true)` end up). Actions: list, poll, log, wait, kill,
//! write, submit, close. Stdout/stderr come from files, so they are also
//! fully readable after the process has ended.

use crate::tools::{tool_error, JsonSchema, Tool, ToolMeta, ToolResult};
use crate::AppState;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct ProcessManage;

fn info_json(i: &crate::process::Info) -> Value {
    json!({
        "id": i.id,
        "pid": i.pid,
        "command": i.command,
        "workdir": i.workdir,
        "running": i.running,
        "exit_code": i.exit_code,
        "uptime_ms": i.uptime_ms as u64,
        "stdout_path": i.stdout_path,
        "stderr_path": i.stderr_path,
    })
}

/// Cap a log fragment before it goes into the tool result (same budget as
/// `terminal`), so a chatty process cannot flood the context.
const MAX_OUT: usize = 50_000;
fn cap(s: String) -> String {
    if s.chars().count() > MAX_OUT {
        let cut: String = s.chars().take(MAX_OUT).collect();
        format!("{cut}...\n[truncated at {MAX_OUT} chars]")
    } else {
        s
    }
}

impl Tool for ProcessManage {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "process_manage",
            description: "Manage background processes from terminal(background:true).",
            emoji: "🖥️",
            toolset: "terminal",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &SCHEMA
    }

    fn call(&self, args: Value, _state: Arc<AppState>) -> ToolResult {
        let action = match args.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => return tool_error("action is required"),
        };

        // list/prune need no id
        if action == "list" {
            let procs: Vec<Value> = crate::process::list().iter().map(info_json).collect();
            return json!({ "processes": procs, "count": procs.len() }).to_string();
        }
        if action == "prune" {
            let removed = crate::process::prune();
            let remaining = crate::process::list().len();
            return json!({
                "removed": removed,
                "remaining": remaining,
                "message": format!(
                    "Pruned {removed} finished process(es); {remaining} still in the registry."
                ),
            })
            .to_string();
        }

        let id = match args.get("session_id").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s,
            _ => return tool_error("session_id is required (id or unique prefix from `list`)"),
        };

        match action {
            "poll" => match crate::process::poll(id) {
                Ok((info, out, err)) => json!({
                    "process": info_json(&info),
                    "new_output": cap(out),
                    "new_stderr": cap(err),
                })
                .to_string(),
                Err(e) => tool_error(e),
            },
            "log" => {
                let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
                match crate::process::log(id, offset, limit) {
                    Ok((info, text, total)) => json!({
                        "process": info_json(&info),
                        "output": cap(text),
                        "total_lines": total,
                        "offset": offset,
                        "limit": limit,
                    })
                    .to_string(),
                    Err(e) => tool_error(e),
                }
            }
            "wait" => {
                let secs = args
                    .get("timeout")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30)
                    .min(600);
                match crate::process::wait(id, secs * 1000) {
                    Ok(info) => {
                        let still_running = info.running;
                        let mut v = json!({ "process": info_json(&info) });
                        if still_running {
                            v["note"] =
                                json!(format!("still running after {secs}s — poll again or kill"));
                        }
                        v.to_string()
                    }
                    Err(e) => tool_error(e),
                }
            }
            "kill" => match crate::process::kill(id) {
                Ok(info) => json!({ "process": info_json(&info), "killed": true }).to_string(),
                Err(e) => tool_error(e),
            },
            "write" | "submit" => {
                let data = args.get("data").and_then(|v| v.as_str()).unwrap_or("");
                match crate::process::write_stdin(id, data, action == "submit") {
                    Ok(()) => json!({ "sent": data, "submit": action == "submit" }).to_string(),
                    Err(e) => tool_error(e),
                }
            }
            "close" => match crate::process::close_stdin(id) {
                Ok(()) => json!({ "stdin_closed": true }).to_string(),
                Err(e) => tool_error(e),
            },
            other => tool_error(format!(
                "Unknown action: {other} (list|poll|log|wait|kill|write|submit|close)"
            )),
        }
    }
}

lazy_static::lazy_static! {
    static ref SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["list", "poll", "log", "wait", "kill", "write", "submit", "close", "prune"],
                "description": "list | poll (new output) | log (paged) | wait | kill | write/submit (stdin) | close (stdin EOF) | prune (drop finished)"
            },
            "session_id": { "type": "string", "description": "id or unique prefix (from list)" },
            "timeout": { "type": "integer", "description": "wait: seconds (max 600)", "minimum": 1, "maximum": 600 },
            "offset": { "type": "integer", "description": "log: first line (1-based)", "minimum": 1 },
            "limit": { "type": "integer", "description": "log: max lines", "minimum": 1 },
            "data": { "type": "string", "description": "write/submit: stdin text" }
        },
        "required": ["action"]
    }));
}
