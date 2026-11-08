//! Dogfoods the MCP client against Rijo Code's own MCP server (ADR 0017).
//!
//! The server (`rijo mcp serve`) is already verified in `docs/MCP.md`, so a
//! green run here proves the *client* half of the protocol without a third-party
//! dependency — and offline. The server is headless: no onboarding, no trust
//! prompt, no LLM.

use rijo_core::mcp_client::McpClient;
use serde_json::json;

fn rijo_binary() -> &'static str {
    env!("CARGO_BIN_EXE_rijo")
}

async fn connect() -> McpClient {
    McpClient::connect_stdio(
        rijo_binary(),
        &["mcp".to_string(), "serve".to_string()],
        &[],
    )
    .await
    .expect("connect to `rijo mcp serve`")
}

#[tokio::test]
async fn lists_the_servers_meta_tools() {
    let client = connect().await;
    let tools = client.list_tools().await.expect("list_tools");

    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"list_tools"), "got {names:?}");
    assert!(names.contains(&"call_tool"), "got {names:?}");

    // Every advertised tool carries a JSON input schema the model can act on.
    for tool in &tools {
        assert!(
            tool.input_schema.is_object(),
            "`{}` has no object schema: {}",
            tool.name,
            tool.input_schema
        );
    }
}

#[tokio::test]
async fn calls_a_tool_through_the_meta_tool() {
    let client = connect().await;

    // The server's `list_tools` meta-tool returns the native tools as JSON.
    let out = client
        .call("list_tools", json!({}))
        .await
        .expect("call list_tools");

    assert!(out.contains("read_file"), "native tools missing: {out}");
    assert!(out.contains("\"count\""), "count missing: {out}");
}
