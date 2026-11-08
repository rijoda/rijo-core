//! Tool Registry — high-performance, zero-copy tool dispatch
//!
//! Every tool is a `Box<dyn Tool>` registered by name with a JSON Schema.
//! Dispatch is O(1) via an `AHashMap` and runs on tokio's blocking threadpool
//! for sync tools or the async runtime for async tools.

mod arxiv_tool;
mod box_tools;
mod brain_outline_tool;
mod brain_tool;
mod browser_tool;
mod build_hint;
mod clarify_tool;
pub mod context_tool;
mod cron_tool;
pub mod delegate_tool;
mod execute_tool;
pub(crate) mod file_tools;
pub mod git_tool;
pub mod github_tool;
mod mcp_tool;
mod memory_tool;
mod process_tool;
pub mod session_tool;
pub mod skills_tool;
pub(crate) mod terminal_tool;
pub mod todo_tool;
mod tts_tool;
mod vision_tool;
mod web_tools;

pub use memory_tool::list_entries as memory_entries;

use crate::debug;
use crate::AppState;
use ahash::AHashMap;
use std::sync::Arc;

/// A tool error result — structured JSON, never a panic.
pub fn tool_error(msg: impl Into<String>) -> String {
    let msg = msg.into();
    serde_json::json!({"error": msg}).to_string()
}

/// Tool result as JSON string
pub type ToolResult = String;

/// JSON Schema for a tool's arguments
#[derive(Debug, Clone)]
pub struct JsonSchema {
    raw: serde_json::Value,
}

impl JsonSchema {
    pub fn new(schema: serde_json::Value) -> Self {
        Self { raw: schema }
    }

    pub fn raw(&self) -> &serde_json::Value {
        &self.raw
    }
}

/// Metadata about a registered tool
#[derive(Debug, Clone)]
pub struct ToolMeta {
    pub name: &'static str,
    pub description: &'static str,
    pub emoji: &'static str,
    pub toolset: &'static str,
    pub is_async: bool,
}

/// A registered tool handler
pub trait Tool: Send + Sync {
    fn meta(&self) -> &ToolMeta;
    fn schema(&self) -> &JsonSchema;
    fn call(&self, args: serde_json::Value, state: Arc<AppState>) -> ToolResult;
}

/// Type alias for async tool handlers
#[async_trait::async_trait]
pub trait AsyncTool: Send + Sync {
    fn meta(&self) -> &ToolMeta;
    fn schema(&self) -> &JsonSchema;
    async fn call_async(&self, args: serde_json::Value, state: Arc<AppState>) -> ToolResult;
}

/// Registry of all available tools — O(1) dispatch via AHashMap
pub struct Registry {
    tools: AHashMap<&'static str, RegistryEntry>,
}

enum RegistryEntry {
    Sync(Arc<dyn Tool>),
    Async(Arc<dyn AsyncTool>),
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            tools: AHashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.meta().name;
        self.tools.insert(name, RegistryEntry::Sync(tool));
    }

    pub fn register_async(&mut self, tool: Arc<dyn AsyncTool>) {
        let name = tool.meta().name;
        self.tools.insert(name, RegistryEntry::Async(tool));
    }

    /// All registered tool names — sorted alphabetically.
    ///
    /// The order is part of the LLM prompt (the tool schemas are sent in
    /// this order). `AHashMap` scatters the iteration order
    /// per process, so the provider saw a different
    /// prefix on every start: the prompt cache then only hit the system prompt
    /// (measured 1024 of 3967 tokens instead of ~3900).
    pub fn get_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.tools.keys().copied().collect();
        names.sort_unstable();
        names
    }

    pub fn get_schema(&self, name: &str) -> Option<&JsonSchema> {
        match self.tools.get(name)? {
            RegistryEntry::Sync(t) => Some(t.schema()),
            RegistryEntry::Async(t) => Some(t.schema()),
        }
    }

    pub fn get_meta(&self, name: &str) -> Option<ToolMeta> {
        match self.tools.get(name)? {
            RegistryEntry::Sync(t) => Some(t.meta().clone()),
            RegistryEntry::Async(t) => Some(t.meta().clone()),
        }
    }

    /// Dispatch a tool call — the main bottleneck-elimination path
    ///
    /// Sync tools run on `tokio::task::spawn_blocking` (uses the dedicated
    /// threadpool). Async tools run directly on the tokio runtime.
    pub async fn dispatch(
        &self,
        name: &str,
        args: serde_json::Value,
        state: Arc<AppState>,
    ) -> ToolResult {
        let entry = match self.tools.get(name) {
            Some(e) => e,
            None => return tool_error(format!("Unknown tool: {name}")),
        };

        match entry {
            RegistryEntry::Sync(tool) => {
                let tool = tool.clone(); // Arc<dyn Tool> — 'static for the closure
                let args_clone = args.clone();
                let state_clone = state.clone();
                tokio::task::spawn_blocking(move || {
                    let start = std::time::Instant::now();
                    let result = tool.call(args_clone.clone(), state_clone);
                    debug::log_call(tool.meta().name, &args_clone, start.elapsed(), &result);
                    result
                })
                .await
                .unwrap_or_else(|e| tool_error(format!("Tool panicked: {e}")))
            }
            RegistryEntry::Async(tool) => {
                let start = std::time::Instant::now();
                let result = tool.call_async(args.clone(), state).await;
                debug::log_call(name, &args, start.elapsed(), &result);
                result
            }
        }
    }

    /// Build the default registry with all built-in tools registered
    pub fn build_default() -> Self {
        let mut r = Self::new();

        // Register sync tools
        r.register(Arc::new(terminal_tool::TerminalTool));
        r.register(Arc::new(git_tool::GitTool));
        r.register(Arc::new(file_tools::ReadFileTool));
        r.register(Arc::new(file_tools::WriteFileTool));
        r.register(Arc::new(file_tools::PatchTool));
        r.register(Arc::new(file_tools::SearchFilesTool));
        r.register(Arc::new(file_tools::DeleteFileTool));
        r.register(Arc::new(skills_tool::SkillsList));
        r.register(Arc::new(skills_tool::SkillView));
        r.register(Arc::new(skills_tool::SkillManage));
        r.register(Arc::new(memory_tool::MemoryTool));
        r.register(Arc::new(brain_tool::BrainSearch));
        r.register(Arc::new(brain_outline_tool::BrainOutline));
        r.register(Arc::new(todo_tool::TodoTool));
        r.register(Arc::new(clarify_tool::ClarifyTool));
        r.register(Arc::new(process_tool::ProcessManage));
        r.register(Arc::new(context_tool::ContextTool));
        r.register_async(Arc::new(github_tool::GithubTool));
        r.register_async(Arc::new(mcp_tool::McpTool));

        // Register async tools
        r.register_async(Arc::new(web_tools::WebSearch));
        r.register_async(Arc::new(web_tools::WebExtract));
        r.register_async(Arc::new(arxiv_tool::ArxivTool));
        r.register_async(Arc::new(browser_tool::BrowserNavigate));
        r.register_async(Arc::new(browser_tool::BrowserSnapshot));
        r.register_async(Arc::new(browser_tool::BrowserClick));
        r.register_async(Arc::new(browser_tool::BrowserType));
        r.register_async(Arc::new(browser_tool::BrowserScroll));
        r.register_async(Arc::new(browser_tool::BrowserDev));
        r.register_async(Arc::new(session_tool::SessionSearch));
        r.register_async(Arc::new(execute_tool::ExecuteCode));
        r.register_async(Arc::new(delegate_tool::DelegateTask));
        r.register_async(Arc::new(cron_tool::CronManage));
        r.register_async(Arc::new(vision_tool::VisionAnalyze));
        r.register_async(Arc::new(tts_tool::TextToSpeech));

        // The `box` toolset is config-gated: it is registered only when a box is configured
        // (`RIJO_BOX_ADDR`), so it costs no prompt weight otherwise (ADR 0011).
        if crate::box_client::BoxClient::from_env().is_some() {
            box_tools::register(&mut r);
        }

        r
    }
}

// Re-export everything for convenient access
pub use browser_tool::*;
pub use clarify_tool::*;
pub use cron_tool::*;
pub use delegate_tool::*;
pub use execute_tool::*;
pub use file_tools::*;
pub use memory_tool::*;
pub use process_tool::*;
pub use session_tool::*;
pub use skills_tool::*;
pub use terminal_tool::*;
pub use todo_tool::*;
pub use tts_tool::*;
pub use vision_tool::*;
pub use web_tools::*;
