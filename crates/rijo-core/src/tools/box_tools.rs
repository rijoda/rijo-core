//! The `box` toolset (ADR 0011) — thin clients over the box.v1 tool socket.
//!
//! Each tool forwards to `rijo-box serve` and returns its JSON. The toolset is registered only
//! when a box is configured (`RIJO_BOX_ADDR`), so it costs no prompt weight otherwise (the
//! planning's "config-gated block"). Names are static; `box_id` is an argument.
//!
//! Blast radius (ADR 0011 §5, ADR 0019): in the P0 workspace-share model the `fs.*` tools are
//! **host-crossing**. They go through **Rijo Code's guard** like `terminal`/`mcp` (agent rule 3):
//! `box_fs_write`/`box_fs_delete` call `guard::approve_async` — denied by default headless and
//! audited via `guard_trace` — while the read-only tools (`box_fs_read`/`list`,
//! `box_status/list/metrics`) are not approval-gated, the same read-only split as the `mcp` tool.
//! The **box's own guard** is the second layer: it denies by default and audit-logs the enforcement.

use crate::box_client::BoxClient;
use crate::tools::{tool_error, AsyncTool, JsonSchema, Registry, ToolMeta, ToolResult};
use crate::AppState;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

lazy_static::lazy_static! {
    static ref STATUS_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": { "box_id": { "type": "string", "description": "Box id (P0: bx-0)." } },
        "required": []
    }));
    static ref LIST_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object", "properties": {}, "required": []
    }));
    static ref METRICS_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": { "box_id": { "type": "string" } },
        "required": []
    }));
    static ref FS_LIST_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "box_id": { "type": "string" },
            "path": { "type": "string", "description": "Workspace directory (P0: the root)." }
        },
        "required": []
    }));
    static ref FS_READ_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "box_id": { "type": "string" },
            "path": { "type": "string", "description": "Path relative to the box workspace." },
            "max_bytes": { "type": "integer" }
        },
        "required": ["path"]
    }));
    static ref FS_WRITE_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "box_id": { "type": "string" },
            "path": { "type": "string" },
            "content": { "type": "string" }
        },
        "required": ["path", "content"]
    }));
    static ref FS_DELETE_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "box_id": { "type": "string" },
            "path": { "type": "string" }
        },
        "required": ["path"]
    }));
    static ref EXEC_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "box_id": { "type": "string" },
            "cmd": { "type": "string", "description": "Command for the in-guest agent." },
            "cwd": { "type": "string" },
            "timeout_ms": { "type": "integer" }
        },
        "required": ["cmd"]
    }));
    static ref NAVIGATE_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "box_id": { "type": "string" },
            "url": { "type": "string" }
        },
        "required": ["url"]
    }));
    static ref SCREENSHOT_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "box_id": { "type": "string" },
            "region": { "type": "string" },
            "max_edge": { "type": "integer" }
        },
        "required": []
    }));
    static ref FIND_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "box_id": { "type": "string" },
            "role": { "type": "string" },
            "name": { "type": "string" }
        },
        "required": []
    }));
    static ref CLICK_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "box_id": { "type": "string" },
            "x": { "type": "number" },
            "y": { "type": "number" }
        },
        "required": ["x", "y"]
    }));
    static ref TYPE_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "box_id": { "type": "string" },
            "text": { "type": "string" }
        },
        "required": ["text"]
    }));
    static ref SCROLL_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "box_id": { "type": "string" },
            "dx": { "type": "number" },
            "dy": { "type": "number" }
        },
        "required": ["dx", "dy"]
    }));
}

/// Attach the static tool name and forward the flat request to the box.
fn forward(state: &AppState, tool: &'static str, mut args: Value) -> ToolResult {
    let Some(client) = BoxClient::from_config(&state.config) else {
        return tool_error("no box configured (set RIJO_BOX_ADDR or [box] addr)");
    };
    let Some(obj) = args.as_object_mut() else {
        return tool_error("bad args: expected a JSON object");
    };
    obj.insert("tool".to_string(), Value::String(tool.to_string()));
    match client.call(&args.to_string()) {
        Ok(resp) => resp,
        Err(e) => tool_error(e),
    }
}

/// One tool per struct — static name, schema and toolset (`box`), async dispatch.
///
/// `$guarded` tools (those that mutate the box workspace) pass Rijo Code's guard first; the
/// read-only ones forward directly (the same split as the `mcp` tool).
macro_rules! box_tool {
    ($ty:ident, $name:literal, $desc:literal, $emoji:literal, $schema:ident, $guarded:expr) => {
        pub struct $ty;

        #[async_trait]
        impl AsyncTool for $ty {
            fn meta(&self) -> &ToolMeta {
                &ToolMeta {
                    name: $name,
                    description: $desc,
                    emoji: $emoji,
                    toolset: "box",
                    is_async: true,
                }
            }
            fn schema(&self) -> &JsonSchema {
                &$schema
            }
            async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
                if $guarded {
                    let detail = format!("{}({})", $name, args);
                    if let Err(e) = crate::guard::approve_async(
                        &state,
                        $name,
                        &detail,
                        "writes to the box workspace (host-crossing)",
                    )
                    .await
                    {
                        return tool_error(e);
                    }
                }
                forward(&state, $name, args)
            }
        }
    };
}

box_tool!(
    BoxStatus,
    "box_status",
    "Box status: state, workspace, approval and guard counters.",
    "📦",
    STATUS_SCHEMA,
    false
);
box_tool!(
    BoxList,
    "box_list",
    "List the boxes.",
    "📦",
    LIST_SCHEMA,
    false
);
box_tool!(
    BoxMetrics,
    "box_metrics",
    "Host-side box counters (guard decisions).",
    "📦",
    METRICS_SCHEMA,
    false
);
box_tool!(
    BoxFsList,
    "box_fs_list",
    "List the box workspace root.",
    "📁",
    FS_LIST_SCHEMA,
    false
);
box_tool!(
    BoxFsRead,
    "box_fs_read",
    "Read a file from the box workspace.",
    "📁",
    FS_READ_SCHEMA,
    false
);
box_tool!(
    BoxFsWrite,
    "box_fs_write",
    "Write a file into the box workspace.",
    "📁",
    FS_WRITE_SCHEMA,
    true
);
box_tool!(
    BoxFsDelete,
    "box_fs_delete",
    "Delete a file from the box workspace.",
    "📁",
    FS_DELETE_SCHEMA,
    true
);
// Guest-only (ADR 0011 §5): the VM is the boundary, so this is audit-logged, not approval-gated
// (ADR 0018 — in-box exec is contained).
box_tool!(
    BoxExec,
    "box_exec",
    "Run a command in the box's in-guest agent.",
    "▶",
    EXEC_SCHEMA,
    false
);
// P1 visual pipeline (ADR 0014): guest-only, contained by the VM — not approval-gated.
box_tool!(
    BoxNavigate,
    "box_navigate",
    "Navigate the in-box browser to a URL.",
    "🌐",
    NAVIGATE_SCHEMA,
    false
);
box_tool!(
    BoxScreenshot,
    "box_screenshot",
    "Capture the in-box browser (returns the image path + size).",
    "🖥",
    SCREENSHOT_SCHEMA,
    false
);
box_tool!(
    BoxFind,
    "box_find",
    "Find in-box browser nodes by role/name; returns viewport bounds (Set-of-Marks).",
    "🔎",
    FIND_SCHEMA,
    false
);
box_tool!(
    BoxClick,
    "box_click",
    "Click the in-box browser at viewport (x, y).",
    "🖱",
    CLICK_SCHEMA,
    false
);
box_tool!(
    BoxType,
    "box_type",
    "Type text into the in-box browser.",
    "⌨",
    TYPE_SCHEMA,
    false
);
box_tool!(
    BoxScroll,
    "box_scroll",
    "Scroll the in-box browser by (dx, dy).",
    "↕",
    SCROLL_SCHEMA,
    false
);

/// Register the whole `box` block (sorted by name via the registry's `get_names`).
pub fn register(r: &mut Registry) {
    r.register_async(Arc::new(BoxStatus));
    r.register_async(Arc::new(BoxList));
    r.register_async(Arc::new(BoxMetrics));
    r.register_async(Arc::new(BoxFsList));
    r.register_async(Arc::new(BoxFsRead));
    r.register_async(Arc::new(BoxFsWrite));
    r.register_async(Arc::new(BoxFsDelete));
    r.register_async(Arc::new(BoxExec));
    r.register_async(Arc::new(BoxNavigate));
    r.register_async(Arc::new(BoxScreenshot));
    r.register_async(Arc::new(BoxFind));
    r.register_async(Arc::new(BoxClick));
    r.register_async(Arc::new(BoxType));
    r.register_async(Arc::new(BoxScroll));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_expose_static_names_and_the_box_toolset() {
        assert_eq!(BoxStatus.meta().name, "box_status");
        assert_eq!(BoxFsRead.meta().name, "box_fs_read");
        assert_eq!(BoxFsRead.meta().toolset, "box");
        assert!(BoxFsRead.meta().is_async);
    }

    #[test]
    fn register_adds_the_whole_block_sorted() {
        let mut r = Registry::new();
        register(&mut r);
        let names = r.get_names();
        let box_names: Vec<&str> = names
            .iter()
            .copied()
            .filter(|n| n.starts_with("box_"))
            .collect();
        assert_eq!(
            box_names,
            vec![
                "box_click",
                "box_exec",
                "box_find",
                "box_fs_delete",
                "box_fs_list",
                "box_fs_read",
                "box_fs_write",
                "box_list",
                "box_metrics",
                "box_navigate",
                "box_screenshot",
                "box_scroll",
                "box_status",
                "box_type",
            ]
        );
    }

    fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.data_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(&config.data_dir).unwrap();
        let state = Arc::new(AppState {
            brain: Arc::new(
                crate::brain::db::BrainDb::open(&config.data_dir.join("brain.db"), 384).unwrap(),
            ),
            embedder: tokio::sync::Mutex::new(None),
            config,
            session_id: crate::AppState::new_session_id(),
            tools: Arc::new(Registry::new()),
            ui_tx: std::sync::Mutex::new(None),
            files_read: std::sync::Mutex::new(std::collections::HashSet::new()),
            cancel: std::sync::atomic::AtomicBool::new(false),
            router: std::sync::Mutex::new(None),
        });
        (state, dir)
    }

    // Headless (no TUI) the guard denies a mutating box action before any box contact —
    // the same rule as `terminal`/`mcp` (agent rule 3).
    #[tokio::test]
    async fn a_headless_write_is_denied_before_it_reaches_the_box() {
        let (state, _dir) = test_state();
        let out = BoxFsWrite
            .call_async(serde_json::json!({"path":"a.txt","content":"x"}), state)
            .await;
        assert!(!out.contains("\"ok\": true"), "must not succeed: {out}");
        assert!(out.contains("BLOCKED"), "expected a guard denial: {out}");
    }

    // A read is not approval-gated: it gets past the guard and only fails for lack of a box.
    #[tokio::test]
    async fn a_read_is_not_guard_gated_it_only_needs_a_box() {
        let (state, _dir) = test_state();
        let out = BoxFsRead
            .call_async(serde_json::json!({"path":"a.txt"}), state)
            .await;
        assert!(out.contains("no box configured"), "{out}");
    }
}
