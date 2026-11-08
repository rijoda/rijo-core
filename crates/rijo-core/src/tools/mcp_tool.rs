//! `mcp` — consume external Model Context Protocol servers (ADR 0017).
//!
//! One stable tool with three actions keeps the tool schema (and therefore the
//! prompt prefix) constant, so the alphabetical tool order and the provider
//! cache stay intact:
//!
//! - `list_servers` — which servers are configured (no connection)
//! - `list_tools`   — one server's tools and their JSON schemas
//! - `call`         — run one remote tool by name
//!
//! Guard policy (ADR 0017): a remote tool has no native guard of its own, so a
//! call runs without asking only when its name is in the server's
//! `read_only_tools`; everything else goes through `guard::approve_async`
//! (TUI dialog, denied by default headless — `RIJO_APPROVE_DANGEROUS=1` is the
//! explicit pre-approval, exactly as for `terminal`/`execute_code`).
//!
//! Connections are opened per call. That is simple and always fresh; pooling
//! them is a later step (documented as open in `docs/MCP.md`).

use crate::config::McpServerConfig;
use crate::guard;
use crate::mcp_client::McpClient;
use crate::tools::{tool_error, AsyncTool, JsonSchema, ToolMeta, ToolResult};
use crate::AppState;
use serde_json::{json, Value};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

pub struct McpTool;

/// Bounds the spawn + MCP handshake so a broken server cannot hang a turn.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(45);
/// Bound server requests too: some servers complete initialize and then hang
/// forever while answering `tools/list` or `tools/call`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

async fn bounded_request<T>(
    server: &str,
    operation: &str,
    timeout: Duration,
    future: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::time::timeout(timeout, future).await.map_err(|_| {
        format!(
            "MCP server `{server}` timed out during {operation} after {} s",
            timeout.as_secs()
        )
    })?
}

lazy_static::lazy_static! {
    static ref MCP_META: ToolMeta = ToolMeta {
        name: "mcp",
        description: "Consume external MCP (Model Context Protocol) servers configured under `mcp.servers`. Actions: list_servers (configured servers), list_tools (a server's tools and their JSON schemas), call (run one remote tool). Only tools listed in a server's read_only_tools run without approval; every other call needs the user's confirmation (denied by default in headless runs).",
        emoji: "🔌",
        toolset: "integration",
        is_async: true,
    };

    static ref MCP_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "description": "list_servers = configured MCP servers; list_tools = one server's tools and their JSON schemas; call = run one remote tool by name.",
                "enum": ["list_servers", "list_tools", "call"]
            },
            "server": {
                "type": "string",
                "description": "Configured server name (see list_servers). Required for list_tools and call."
            },
            "tool": {
                "type": "string",
                "description": "Remote tool name for 'call' (see list_tools)."
            },
            "arguments": {
                "type": "object",
                "description": "JSON object of arguments for 'call' (must match the tool's schema from list_tools)."
            }
        },
        "required": ["action"]
    }));
}

#[async_trait::async_trait]
impl AsyncTool for McpTool {
    fn meta(&self) -> &ToolMeta {
        &MCP_META
    }

    fn schema(&self) -> &JsonSchema {
        &MCP_SCHEMA
    }

    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        match action {
            "list_servers" => list_servers(&state),
            "list_tools" => match server_arg(&args, &state) {
                Err(e) => e,
                Ok(server) => connect_and_list(server).await,
            },
            "call" => match server_arg(&args, &state) {
                Err(e) => e,
                Ok(server) => call_remote(server, &args, &state).await,
            },
            other => tool_error(format!(
                "unknown action `{other}`; use list_servers, list_tools or call"
            )),
        }
    }
}

/// The configured servers, without opening a connection.
fn list_servers(state: &AppState) -> ToolResult {
    let servers: Vec<Value> = state
        .config
        .mcp
        .servers
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "command": s.command,
                "args": s.args,
                "enabled": s.enabled,
                "read_only_tools": s.read_only_tools,
            })
        })
        .collect();
    json!({ "count": servers.len(), "servers": servers }).to_string()
}

/// The named, enabled server — or a model-readable error.
fn server_arg<'a>(args: &Value, state: &'a AppState) -> Result<&'a McpServerConfig, String> {
    let name = match args.get("server").and_then(|v| v.as_str()) {
        Some(name) => name,
        None => return Err(tool_error("`server` is required for list_tools and call")),
    };
    state.config.mcp.server(name).ok_or_else(|| {
        tool_error(format!(
            "unknown or disabled MCP server `{name}` — see action `list_servers`"
        ))
    })
}

fn env_pairs(server: &McpServerConfig) -> Vec<(String, String)> {
    server
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Connect with a bounded timeout so a dead server costs time, not a turn.
async fn connect(server: &McpServerConfig) -> Result<McpClient, String> {
    tokio::time::timeout(
        CONNECT_TIMEOUT,
        McpClient::connect_stdio(&server.command, &server.args, &env_pairs(server)),
    )
    .await
    .map_err(|_| {
        format!(
            "MCP server `{}` did not respond within {} s",
            server.name,
            CONNECT_TIMEOUT.as_secs()
        )
    })?
}

async fn connect_and_list(server: &McpServerConfig) -> ToolResult {
    let client = match connect(server).await {
        Ok(client) => client,
        Err(e) => return tool_error(e),
    };
    match bounded_request(
        &server.name,
        "list_tools",
        REQUEST_TIMEOUT,
        client.list_tools(),
    )
    .await
    {
        Ok(tools) => {
            let tools: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect();
            json!({ "server": server.name, "count": tools.len(), "tools": tools }).to_string()
        }
        Err(e) => tool_error(e),
    }
}

async fn call_remote(server: &McpServerConfig, args: &Value, state: &AppState) -> ToolResult {
    let tool = match args.get("tool").and_then(|v| v.as_str()) {
        Some(tool) => tool,
        None => return tool_error("`tool` is required for action `call`"),
    };
    let arguments = args.get("arguments").cloned().unwrap_or(Value::Null);

    // Guard first, connection second: an undeclared remote tool never even
    // spawns a process when the user (or the headless default) declines.
    if !server.is_read_only(tool) {
        let detail = format!("{} -> {}({})", server.name, tool, arguments);
        if let Err(e) = guard::approve_async(
            state,
            "mcp",
            &detail,
            "remote MCP tool not declared read-only",
        )
        .await
        {
            return tool_error(e);
        }
    }

    let client = match connect(server).await {
        Ok(client) => client,
        Err(e) => return tool_error(e),
    };
    match bounded_request(
        &server.name,
        &format!("tool `{tool}`"),
        REQUEST_TIMEOUT,
        client.call(tool, arguments),
    )
    .await
    {
        Ok(text) => json!({ "server": server.name, "tool": tool, "result": text }).to_string(),
        Err(e) => tool_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::bounded_request;
    use std::time::Duration;

    #[tokio::test]
    async fn nonresponsive_mcp_requests_return_a_bounded_error() {
        let result = bounded_request(
            "hung-server",
            "tool `never_returns`",
            Duration::from_millis(10),
            async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok::<_, String>(())
            },
        )
        .await;

        let error = result.unwrap_err();
        assert!(error.contains("hung-server"), "{error}");
        assert!(error.contains("never_returns"), "{error}");
        assert!(error.contains("timed out"), "{error}");
    }
}
