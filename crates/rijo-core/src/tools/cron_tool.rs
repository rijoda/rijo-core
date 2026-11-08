//! Cron tool — file-based scheduled jobs
//!
//! Jobs live in ~/.rijo/data/cron/jobs/<id>.json. A scheduler loop
//! (`src/cli.rs::cron_loop`) is expected to poll due jobs and execute them.
//! Execution spawns `rijo --child` with the job prompt — same isolation
//! as delegate_task.

use crate::tools::{tool_error, AsyncTool, JsonSchema, ToolMeta, ToolResult};
use crate::AppState;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct CronManage;

/// Interval seconds for supported shorthand schedules
fn parse_schedule(schedule: &str) -> Option<u64> {
    match schedule.trim() {
        "30m" => Some(1800),
        "1h" => Some(3600),
        "2h" => Some(7200),
        s if s.ends_with('m') => s.trim_end_matches('m').parse().map(|m: u64| m * 60).ok(),
        s => s.parse().ok(), // raw seconds
    }
}

#[async_trait]
impl AsyncTool for CronManage {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "cronjob_manage",
            description: "Create/list/remove scheduled jobs. Schedule: '30m', '1h', seconds.",
            emoji: "⏰",
            toolset: "core",
            is_async: true,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &CRON_SCHEMA
    }

    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list");
        let dir = state.config.data_dir.join("cron").join("jobs");
        let _ = std::fs::create_dir_all(&dir);

        match action {
            "create" => {
                let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
                    Some(p) if !p.is_empty() => p,
                    _ => return tool_error("prompt is required for create"),
                };
                let schedule = args
                    .get("schedule")
                    .and_then(|v| v.as_str())
                    .unwrap_or("1h");
                let interval = match parse_schedule(schedule) {
                    Some(s) if s >= 60 => s,
                    _ => return tool_error("schedule must be >=60s: '30m','1h','2h', or seconds"),
                };

                let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
                let job = json!({
                    "id": id,
                    "prompt": prompt,
                    "interval_secs": interval,
                    "created_at": chrono::Utc::now().to_rfc3339(),
                    "next_run": chrono::Utc::now().timestamp() + interval as i64,
                    "runs": 0,
                });
                let path = dir.join(format!("{id}.json"));
                match std::fs::write(&path, serde_json::to_string_pretty(&job).unwrap()) {
                    Ok(()) => {}
                    Err(e) => return tool_error(format!("write job: {e}")),
                }

                json!({"created": true, "id": id, "interval_secs": interval}).to_string()
            }
            "list" => {
                let mut jobs = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for e in entries.flatten() {
                        if let Ok(c) = std::fs::read_to_string(e.path()) {
                            if let Ok(job) = serde_json::from_str::<Value>(&c) {
                                jobs.push(job);
                            }
                        }
                    }
                }
                json!({ "jobs": jobs, "count": jobs.len() }).to_string()
            }
            "remove" => {
                let job_id = args.get("job_id").and_then(|v| v.as_str()).unwrap_or("");
                if job_id.is_empty() {
                    return tool_error("job_id is required for remove");
                }
                match std::fs::remove_file(dir.join(format!("{job_id}.json"))) {
                    Ok(()) => json!({"removed": true, "id": job_id}).to_string(),
                    Err(_) => tool_error(format!("job not found: {job_id}")),
                }
            }
            _ => tool_error(format!("unknown action: {action}")),
        }
    }
}

lazy_static::lazy_static! {
    static ref CRON_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["create", "list", "remove"], "description": "Cron action" },
            "prompt": { "type": "string", "description": "Job prompt (create)" },
            "schedule": { "type": "string", "description": "'30m','1h' or seconds" },
            "job_id": { "type": "string", "description": "Job id (remove)" }
        }
    }));
}
