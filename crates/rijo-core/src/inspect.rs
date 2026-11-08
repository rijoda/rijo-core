//! Read-only inspection API for any frontend.
//!
//! The desktop workbench used to build these answers itself (tool catalog,
//! runtime facts, panel data, workspace status). That duplicated core knowledge
//! in the GUI and made the GUI a second implementation of the backend. The
//! functions here are the single source: the TUI, the desktop app and any future
//! client call them, and the core stays useful without a particular UI.

use crate::tools::ToolResult;
use crate::AppState;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

/// Is a provider configured (a stored key, a key discoverable from the env, or a
/// local base URL)?
pub fn provider_configured(config: &crate::config::Config) -> bool {
    !config.llm.api_key.trim().is_empty()
        || crate::local_server::is_local_base(&config.llm.base_url)
        || crate::providers::all()
            .iter()
            .any(|provider| crate::providers::resolve_token(provider, "").is_some())
}

/// Every registered tool with the metadata a catalog needs.
pub fn tool_catalog(state: &AppState) -> Vec<Value> {
    state
        .tools
        .get_names()
        .into_iter()
        .filter_map(|name| {
            let meta = state.tools.get_meta(name)?;
            let schema = state.tools.get_schema(name)?;
            Some(json!({
                "name": meta.name,
                "toolset": meta.toolset,
                "description": meta.description,
                "async": meta.is_async,
                "schema": schema.raw(),
            }))
        })
        .collect()
}

/// Model, provider, sandbox and session facts — no credentials.
pub fn runtime_info(state: &AppState) -> Value {
    let config = &state.config;
    json!({
        "workspace": display_path(&config.workspace),
        "model": config.llm.model,
        "providerConfigured": provider_configured(config),
        "contextWindow": config.llm.effective_context_window().unwrap_or(0),
        "registeredTools": state.tools.get_names().len(),
        "sandboxBackend": config.sandbox.backend,
        "sessionId": state.active_session_id(),
    })
}

async fn dispatch_panel_tool(
    state: &Arc<AppState>,
    name: &str,
    args: Value,
) -> Result<Value, String> {
    let raw: ToolResult = state.tools.dispatch(name, args, state.clone()).await;
    let value: Value =
        serde_json::from_str(&raw).map_err(|_| format!("{name} returned an invalid response"))?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(error.to_string());
    }
    Ok(value)
}

/// Data for the read-only panels. Writes are deliberately absent: approvals and
/// questions are the only mutations a frontend may initiate, and those go
/// through `ui_events`.
pub async fn panel(state: &Arc<AppState>, panel: &str, query: &str) -> Result<Value, String> {
    match panel {
        "history" => {
            if query.trim().is_empty() {
                return Ok(json!({ "results": [], "count": 0 }));
            }
            dispatch_panel_tool(
                state,
                "session_search",
                json!({ "query": query, "limit": 20 }),
            )
            .await
        }
        "brain" => {
            if query.trim().is_empty() {
                return Ok(json!({ "results": [], "count": 0 }));
            }
            dispatch_panel_tool(
                state,
                "brain_search",
                json!({ "query": query, "all_workspaces": false }),
            )
            .await
        }
        "skills" => {
            if query.trim().is_empty() {
                dispatch_panel_tool(state, "skills_list", json!({})).await
            } else {
                dispatch_panel_tool(state, "skill_view", json!({ "name": query.trim() })).await
            }
        }
        "memory" => Ok(json!({
            "user": crate::tools::memory_entries(state, "user"),
            "agent": crate::tools::memory_entries(state, "agent"),
        })),
        "operations" => {
            let processes =
                dispatch_panel_tool(state, "process_manage", json!({ "action": "list" })).await?;
            let todos = dispatch_panel_tool(state, "todo_list", json!({})).await?;
            let mut schedules =
                dispatch_panel_tool(state, "cronjob_manage", json!({ "action": "list" })).await?;
            // Scheduled-job prompts are not shown to a frontend.
            if let Some(jobs) = schedules.get_mut("jobs").and_then(Value::as_array_mut) {
                for job in jobs {
                    if let Some(object) = job.as_object_mut() {
                        object.remove("prompt");
                    }
                }
            }
            Ok(json!({ "processes": processes, "todos": todos, "schedules": schedules }))
        }
        "context" => dispatch_panel_tool(state, "context", json!({ "action": "status" })).await,
        _ => Err("Unknown or unsupported read-only panel.".into()),
    }
}

/// Strip the Windows verbatim prefix for display.
pub fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

fn git_status(workspace: &Path) -> (String, usize, u32, u32) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["status", "--porcelain=v1", "-b"])
        .output();
    let Ok(output) = output else {
        return ("not a repository".to_string(), 0, 0, 0);
    };
    if !output.status.success() {
        return ("not a repository".to_string(), 0, 0, 0);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut branch = "detached".to_string();
    let (mut ahead, mut behind, mut changes) = (0u32, 0u32, 0usize);
    for (index, line) in text.lines().enumerate() {
        if index == 0 && line.starts_with("## ") {
            let head = line.trim_start_matches("## ");
            let name = head.split("...").next().unwrap_or(head);
            let name = name.split_whitespace().next().unwrap_or(name);
            if !name.is_empty() {
                branch = name.to_string();
            }
            if let Some(open) = head.find('[') {
                if let Some(close) = head[open..].find(']') {
                    for part in head[open + 1..open + close].split(',') {
                        let part = part.trim();
                        if let Some(value) = part.strip_prefix("ahead ") {
                            ahead = value.parse().unwrap_or(0);
                        }
                        if let Some(value) = part.strip_prefix("behind ") {
                            behind = value.parse().unwrap_or(0);
                        }
                    }
                }
            }
        } else if !line.trim().is_empty() {
            changes += 1;
        }
    }
    (branch, changes, ahead, behind)
}

/// Branch, change count, todo progress and running processes for one workspace.
///
/// This is what a sidebar shows at a glance. It shells out to git once; callers
/// should cache it briefly (the desktop keeps it for four seconds).
pub fn workspace_status(workspace: &Path, data_dir: &Path) -> Value {
    let (branch, changes, ahead, behind) = git_status(workspace);
    let (todos_done, todos_total) = crate::tools::todo_tool::counts(data_dir, workspace);
    let processes = crate::process::list()
        .iter()
        .filter(|info| info.running)
        .count();
    json!({
        "branch": branch,
        "changes": changes,
        "ahead": ahead,
        "behind": behind,
        "todosDone": todos_done,
        "todosTotal": todos_total,
        "processes": processes,
    })
}
