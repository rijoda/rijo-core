//! MCP client — consume external Model Context Protocol servers (ADR 0017).
//!
//! Increment 1: stdio child processes only. This module speaks the protocol and
//! nothing else; the guard policy for remote calls lives one layer up, in the
//! `mcp` tool. A broken server must cost time, never behaviour — every failure
//! is returned as `Err` instead of panicking, and the transport is dropped on
//! exit, which kills the child process.
//!
//! Verified against `rmcp` 3.5.0: `transport::TokioChildProcess`,
//! `Peer::list_all_tools`, `Peer::call_tool` (`CallToolRequestParams` →
//! `CallToolResult`).

use std::process::Stdio;

use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, Tool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use serde_json::{Map, Value};

/// One tool advertised by an external MCP server.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalTool {
    pub name: String,
    pub description: Option<String>,
    /// The tool's JSON argument schema, exactly as the server sent it.
    pub input_schema: Value,
}

/// A live connection to one external MCP server (stdio child process).
pub struct McpClient {
    service: RunningService<RoleClient, ()>,
}

impl McpClient {
    /// Spawn a child process that speaks MCP over stdio and initialise the session.
    pub async fn connect_stdio(
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self, String> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args);
        for (key, value) in env {
            cmd.env(key, value);
        }
        // stderr is discarded: for a stdio server stdout *is* the JSON-RPC
        // channel, and a chatty server must not scribble into the TUI. stdin and
        // stdout stay piped (the builder defaults).
        let (transport, _stderr) = TokioChildProcess::builder(cmd)
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not spawn MCP server `{command}`: {e}"))?;
        let service = ()
            .serve(transport)
            .await
            .map_err(|e| format!("MCP handshake with `{command}` failed: {e}"))?;
        Ok(Self { service })
    }

    /// Every tool the server advertises (follows pagination).
    pub async fn list_tools(&self) -> Result<Vec<ExternalTool>, String> {
        let tools = self
            .service
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| format!("MCP list_tools failed: {e}"))?;
        Ok(tools.into_iter().map(external_tool).collect())
    }

    /// Run one tool by name. `arguments` must be a JSON object (or `null`).
    pub async fn call(&self, name: &str, arguments: Value) -> Result<String, String> {
        let arguments = match arguments {
            Value::Null => Map::new(),
            Value::Object(map) => map,
            other => return Err(format!("arguments must be a JSON object, got: {other}")),
        };
        let params = CallToolRequestParams::new(name.to_string()).with_arguments(arguments);
        let result = self
            .service
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| format!("MCP call `{name}` failed: {e}"))?;
        Ok(render_result(&result))
    }
}

fn external_tool(tool: Tool) -> ExternalTool {
    ExternalTool {
        name: tool.name.to_string(),
        description: tool.description.map(|d| d.to_string()),
        input_schema: Value::Object((*tool.input_schema).clone()),
    }
}

/// Flatten a tool result into the text a model can read. Text blocks are joined
/// with newlines; non-text blocks are marked, so nothing disappears silently.
pub fn render_result(result: &CallToolResult) -> String {
    let mut parts = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::Image(_) => parts.push("[image]".to_string()),
            ContentBlock::Audio(_) => parts.push("[audio]".to_string()),
            ContentBlock::Resource(_) | ContentBlock::ResourceLink(_) => {
                parts.push("[resource]".to_string())
            }
            _ => parts.push("[unsupported content]".to_string()),
        }
    }
    if let Some(structured) = &result.structured_content {
        parts.push(structured.to_string());
    }
    let body = parts.join("\n");
    if result.is_error == Some(true) {
        format!("ERROR: {body}")
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(json: Value) -> CallToolResult {
        serde_json::from_value(json).expect("CallToolResult fixture")
    }

    #[test]
    fn text_blocks_are_joined_with_newlines() {
        let r = result(serde_json::json!({
            "content": [
                {"type": "text", "text": "line 1"},
                {"type": "text", "text": "line 2"}
            ]
        }));
        assert_eq!(render_result(&r), "line 1\nline 2");
    }

    #[test]
    fn an_error_result_is_marked() {
        let r = result(serde_json::json!({
            "content": [{"type": "text", "text": "boom"}],
            "isError": true
        }));
        assert_eq!(render_result(&r), "ERROR: boom");
    }

    #[test]
    fn structured_content_is_appended() {
        let r = result(serde_json::json!({
            "content": [],
            "structuredContent": {"count": 3}
        }));
        assert_eq!(render_result(&r), "{\"count\":3}");
    }
}
