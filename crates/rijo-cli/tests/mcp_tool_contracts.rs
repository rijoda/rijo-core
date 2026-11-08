//! The `mcp` tool end-to-end through the real registry (ADR 0017).
//!
//! The configured server is Rijo Code's own `rijo mcp serve`, so the whole path
//! — registry dispatch → guard → client → server — is exercised offline. The
//! guarded case proves the policy: a remote tool that is not declared read-only
//! is denied in a headless run without ever spawning the server.

use rijo_core::config::{Config, McpServerConfig};
use rijo_core::AppState;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// An AppState with one `mcp` server named `self` pointing at this binary's
/// `mcp serve`. `read_only` are the remote tool names allowed without a dialog.
fn state_with_self_server(read_only: &[&str]) -> Arc<AppState> {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let cwd = std::env::current_dir().expect("cwd");
    let mut cfg = Config::default();
    cfg.workspace = cwd.clone();
    // Per-test data dir: parallel tests must not share one SQLite file (§Z).
    cfg.data_dir = cwd
        .join("target")
        .join("test_artifacts")
        .join(format!("mcp_tool_{}", SEQ.fetch_add(1, Ordering::SeqCst)));
    let _ = std::fs::create_dir_all(&cfg.data_dir);
    cfg.mcp.servers = vec![McpServerConfig {
        name: "self".to_string(),
        command: env!("CARGO_BIN_EXE_rijo").to_string(),
        args: vec!["mcp".to_string(), "serve".to_string()],
        env: Default::default(),
        read_only_tools: read_only.iter().map(|s| s.to_string()).collect(),
        enabled: true,
    }];

    let brain = Arc::new(
        rijo_core::brain::db::BrainDb::open(&cfg.data_dir.join("brain_test.db"), 384)
            .expect("test BrainDB"),
    );
    let tools = Arc::new(rijo_core::tools::Registry::build_default());
    Arc::new(AppState {
        brain,
        embedder: tokio::sync::Mutex::new(None),
        config: cfg,
        session_id: rijo_core::AppState::new_session_id(),
        tools,
        ui_tx: std::sync::Mutex::new(None),
        files_read: std::sync::Mutex::new(std::collections::HashSet::new()),
        cancel: std::sync::atomic::AtomicBool::new(false),
        router: std::sync::Mutex::new(None),
    })
}

async fn call_mcp(state: &Arc<AppState>, args: Value) -> Value {
    let raw = state.tools.dispatch("mcp", args, state.clone()).await;
    serde_json::from_str(&raw).unwrap_or(json!({ "raw": raw }))
}

#[tokio::test]
async fn list_servers_reports_the_configuration_without_connecting() {
    let state = state_with_self_server(&[]);
    let out = call_mcp(&state, json!({ "action": "list_servers" })).await;
    assert_eq!(out["count"], 1, "got {out}");
    assert_eq!(out["servers"][0]["name"], "self", "got {out}");
}

#[tokio::test]
async fn list_tools_connects_and_reads_the_schemas() {
    let state = state_with_self_server(&[]);
    let out = call_mcp(&state, json!({ "action": "list_tools", "server": "self" })).await;

    let names: Vec<String> = out["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tools in {out}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(names.contains(&"list_tools".to_string()), "got {names:?}");
    assert!(names.contains(&"call_tool".to_string()), "got {names:?}");
}

#[tokio::test]
async fn an_undeclared_remote_tool_is_denied_headless() {
    let state = state_with_self_server(&[]); // nothing declared read-only
    let out = call_mcp(
        &state,
        json!({ "action": "call", "server": "self", "tool": "list_tools", "arguments": {} }),
    )
    .await;

    let err = out["error"].as_str().unwrap_or_default().to_lowercase();
    assert!(
        err.contains("approv") || err.contains("denied"),
        "expected a guard denial, got {out}"
    );
}

#[tokio::test]
async fn a_declared_read_only_tool_runs_without_a_dialog() {
    let state = state_with_self_server(&["list_tools"]);
    let out = call_mcp(
        &state,
        json!({ "action": "call", "server": "self", "tool": "list_tools", "arguments": {} }),
    )
    .await;

    let result = out["result"].as_str().unwrap_or_default();
    assert!(result.contains("read_file"), "native tools missing: {out}");
}
