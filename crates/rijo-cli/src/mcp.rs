//! `rijo mcp serve` — the agent as an MCP server.
//!
//! Exposes the native tool registry to any MCP client (Claude Code, Codex, Cursor, VS Code,
//! Hermes) over stdio. Two meta-tools keep the surface small and the schemas complete:
//!
//! - `list_tools` — every registered tool with its description and JSON argument schema
//! - `call_tool`  — dispatches one tool call by name
//!
//! **Nothing in here may print to stdout.** For a stdio MCP server stdout *is* the JSON-RPC
//! channel; a stray `println!` corrupts the protocol. Diagnostics belong on stderr.
//!
//! Safety: every call goes through `Registry::dispatch`, so a tool's own safety checks are
//! unchanged. The server is headless (`tui_tx` is `None`), so the Guard denies dangerous
//! operations by default — they only run when the server process is started with
//! `RIJO_APPROVE_DANGEROUS=1`.

use std::sync::Arc;

use rijo_core::AppState;
use rmcp::{
    handler::server::wrapper::Parameters, schemars, tool, tool_handler, tool_router,
    transport::stdio, ServerHandler, ServiceExt,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CallParams {
    /// Tool name, exactly as returned by `list_tools`.
    pub name: String,
    /// Tool arguments as a JSON object matching that tool's schema. Omit when a tool
    /// takes no arguments.
    #[serde(default)]
    pub arguments: Option<serde_json::Value>,
}

#[derive(Clone)]
pub struct Rijo {
    state: Arc<AppState>,
}

#[tool_router]
impl Rijo {
    /// Every native tool: name, description, toolset and JSON argument schema.
    #[tool(
        description = "List every native Rijo tool (name, description, toolset, JSON argument \
                       schema). Call this first, then call_tool with one of the names."
    )]
    fn list_tools(&self) -> String {
        let tools: Vec<_> = self
            .state
            .tools
            .get_names()
            .into_iter()
            .filter_map(|name| {
                let meta = self.state.tools.get_meta(name)?;
                let schema = self.state.tools.get_schema(name)?;
                Some(json!({
                    "name": meta.name,
                    "description": meta.description,
                    "toolset": meta.toolset,
                    "async": meta.is_async,
                    "input_schema": schema.raw(),
                }))
            })
            .collect();

        json!({ "count": tools.len(), "tools": tools }).to_string()
    }

    /// Run one native tool by name.
    #[tool(
        description = "Execute one native Rijo tool by name. `arguments` must match the schema \
                       from list_tools. Dangerous operations are denied in headless MCP mode \
                       unless the server runs with RIJO_APPROVE_DANGEROUS=1."
    )]
    async fn call_tool(&self, Parameters(p): Parameters<CallParams>) -> String {
        if self.state.tools.get_schema(&p.name).is_none() {
            return json!({ "error": format!("Unknown tool: {}", p.name) }).to_string();
        }
        let args = p.arguments.unwrap_or_else(|| json!({}));
        self.state
            .tools
            .dispatch(&p.name, args, self.state.clone())
            .await
    }
}

/// What an agent sees when it connects.
#[tool_handler(
    name = "rijo",
    version = "0.1.0",
    instructions = "The Rijo Code agent's native tools: file, search, shell, web, browser, memory \
                    and more. Call `list_tools` once to see them and their JSON schemas, then \
                    `call_tool` with a tool `name` and its `arguments`. Calls run through the same \
                    safety Guard as the agent; dangerous operations are denied by default in \
                    headless MCP mode."
)]
impl ServerHandler for Rijo {}

/// Serve MCP over stdio until the client disconnects.
pub async fn serve(state: Arc<AppState>) -> Result<(), String> {
    let service = Rijo { state }
        .serve(stdio())
        .await
        .map_err(|e| e.to_string())?;
    service.waiting().await.map_err(|e| e.to_string())?;
    Ok(())
}
