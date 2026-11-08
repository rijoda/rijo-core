pub mod bench;
pub mod box_client;
pub mod brain;
pub mod cli;
pub mod compress;
pub mod config;
pub mod debug;
pub mod diagnostics;
pub mod executor;
pub mod guard;
pub mod inspect;
pub mod learning;
pub mod llm;
pub mod local_server;
pub mod mcp_client;
pub mod meta_search;
pub mod metrics;
pub mod models;
pub mod onboarding;
pub mod paths;
pub mod process;
pub mod providers;
pub mod router;
pub mod sandbox;
pub mod schema_eval;
pub mod selfupd;
pub mod summarize;
pub mod sync;
pub mod tech_stack;
pub mod tools;
pub mod trust;
pub mod ui_events;
pub mod watcher;
pub mod web_search;

use std::sync::Arc;

/// Version of the engine crate. The desktop app shows this next to its own
/// version so a bug report names both halves.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared application state accessible by all tool handlers
pub struct AppState {
    pub brain: Arc<brain::db::BrainDb>,
    pub embedder: tokio::sync::Mutex<Option<brain::embed::Embedder>>,
    pub config: config::Config,
    /// Persistence target for the active interactive session. Shared so context
    /// tools and the executor follow the same explicitly resumed session.
    pub session_id: Arc<std::sync::RwLock<String>>,
    pub tools: Arc<tools::Registry>,
    /// UI event channel, set while a frontend is running. Tools use this to ask
    /// for confirmation; the same channel feeds the TUI and the desktop app.
    pub ui_tx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<ui_events::UiEvent>>>,
    /// Files read in the current session — read-before-edit invariant:
    /// patch/write_file on an unread existing file is rejected.
    pub files_read: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Cooperative cancel flag: the UI sets it; the executor checks it per loop
    /// iteration AND per streaming delta. Turn aborts gracefully.
    pub cancel: std::sync::atomic::AtomicBool,
    /// Router session (ONNX encoder + tokenizer). `None` when disabled or when
    /// the model bundle is missing — both mean "send every tool". Scoring
    /// needs `&mut` (the ONNX session is not `Sync`-safe for `run`), hence the
    /// Mutex; the critical section is one forward pass per turn.
    pub router: std::sync::Mutex<Option<router::RouterSession>>,
}

impl AppState {
    /// Default persistence target for a fresh application session.
    pub fn new_session_id() -> Arc<std::sync::RwLock<String>> {
        Arc::new(std::sync::RwLock::new(
            tools::session_tool::current_session_id(),
        ))
    }

    pub fn active_session_id(&self) -> String {
        self.session_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_active_session_id(&self, id: &str) -> Result<(), String> {
        if !(6..=128).contains(&id.len())
            || !id.starts_with("sess_")
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        {
            return Err("invalid saved session id".to_string());
        }
        *self.session_id.write().unwrap_or_else(|e| e.into_inner()) = id.to_string();
        Ok(())
    }
}
