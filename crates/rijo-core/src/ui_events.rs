//! UI contract shared by every frontend of Rijo Code.
//!
//! The core must not depend on a particular presentation. Both the terminal UI
//! and the desktop workbench need the same things from the agent: a transcript
//! model, a live progress stream, and a way to ask the user for approval or a
//! decision. Those types live here, not in `tui.rs`.
//!
//! This module is the *core* half of the contract: it carries only events the
//! engine produces. Terminal input (`Key`, `Mouse`, `Paste`) and clipboard reads
//! belong to the terminal frontend and live in `rijo-tui`, which wraps
//! [`UiEvent`] in its own `TuiEvent`. No terminal or GUI type may appear here —
//! that is what keeps `rijo-core` buildable without a UI toolchain.

use std::sync::Arc;

/// One-shot sender for a confirm action dialog (true=accept, false=reject).
pub type ConfirmTx = tokio::sync::oneshot::Sender<bool>;

/// Role of one transcript entry.
#[derive(Debug, Clone, PartialEq, Hash)]
pub enum EntryRole {
    User,
    Assistant,
    /// live tool activity (⚙ prefix)
    Tool,
    /// live reasoning text (⟡ prefix) — streamed thinking
    Thinking,
    /// A question Rijo asks the user (❓). Prominent: it wants an answer.
    Question,
    /// A todo-list snapshot (✔). Shows where the work stands.
    Todo,
    Error,
    Status,
}

/// One entry in the visible transcript.
#[derive(Debug, Clone)]
pub struct ChatEntry {
    pub role: EntryRole,
    pub text: String,
    /// Full raw detail (tool args/result or reasoning text) revealed by Ctrl+O.
    /// None = plain entry, compact is all there is.
    pub detail: Option<String>,
    /// Thinking-expanded state (k-toggle): true shows full reasoning stream
    pub expanded: bool,
}

/// Everything a frontend can receive from the core while the app is running.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// The todo list changed on disk (`todo_list` wrote it) — refresh the pane.
    TodosChanged,
    /// config.yaml changed on disk
    ConfigReloaded(crate::config::Config),
    /// a skill was created/modified/deleted
    SkillsReloaded,
    /// ~/.rijo/staging/rijo.new updated — binary self-swap offered
    StagedBinary,
    Entry(ChatEntry),
    TurnDone,
    /// live progress from the executor
    Progress(crate::executor::ProgressEvent),
    /// Request user confirmation (one-shot tx sends true/false back)
    ConfirmAction {
        message: String,
        tx: Arc<ConfirmTx>,
    },
    /// Executed turn returned its (grown) conversation history; adopt it
    HistoryUpdate(Vec<crate::llm::ChatMessage>),
    /// A question for the user, asked loudly: the turn WAITS for the answer.
    Ask {
        questions: Vec<AskQuestion>,
        tx: Arc<tokio::sync::oneshot::Sender<Vec<(String, String)>>>,
    },
    /// Results of the asynchronous, local session-history search.
    HistoryResults {
        kind: SearchMode,
        query: String,
        result: Result<Vec<HistoryHit>, String>,
    },
    /// A user-approved resume of one saved session finished loading.
    ResumeLoaded {
        session_id: String,
        result: Result<Vec<(String, String)>, String>,
    },
}

/// One question Rijo asks the user. Kept small on purpose: a decision, a few
/// options, optionally free text.
#[derive(Debug, Clone)]
pub struct AskQuestion {
    pub id: String,
    pub question: String,
    pub options: Vec<AskOption>,
    /// Offer "Anderer …" (free text) after the options. Default when there are
    /// no options at all.
    pub allow_free_text: bool,
}

#[derive(Debug, Clone)]
pub struct AskOption {
    pub label: String,
    pub recommended: bool,
}

/// One hit from the local session-history search.
#[derive(Debug, Clone)]
pub struct HistoryHit {
    pub session_id: String,
    pub role: String,
    pub timestamp: String,
    pub excerpt: String,
}

/// Which store a search targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchMode {
    #[default]
    History,
    Brain,
}
