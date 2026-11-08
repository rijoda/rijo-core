use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Rijo Code configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// LLM provider configuration
    pub llm: LlmConfig,

    /// Memory/brain configuration
    pub memory: MemoryConfig,

    /// Skills directory (Rijo Code's own directory; this is where writes go)
    pub skills_dir: PathBuf,

    /// Additional skill READ roots, e.g. the Hermes library
    /// `~/.hermes/skills`. Reads go across all roots (skills_list,
    /// skill_view, update/patch/delete of an existing skill), while writes
    /// still go only to `skills_dir` — no surprise writes into
    /// someone else's library.
    #[serde(default = "default_skill_roots")]
    pub skill_roots: Vec<PathBuf>,

    /// Workspace root
    pub workspace: PathBuf,

    /// Data directory for sessions, brain, etc.
    pub data_dir: PathBuf,

    /// Terminal timeout in seconds
    pub terminal_timeout: u64,

    /// Max concurrent tools
    pub max_concurrent_tools: usize,

    /// Browser backend ("cdp" or "none")
    pub browser_backend: String,

    /// Local text-to-speech (no cloud backend)
    #[serde(default)]
    pub tts: TtsConfig,

    /// Context budget for the agent loop (tool results + history)
    #[serde(default)]
    pub context: ContextConfig,

    /// Diagnostics feedback after file changes
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,

    /// Learning review after the turn (skill/memory)
    #[serde(default)]
    pub learning: LearningConfig,

    /// Sync with other machines (private repo or rsync)
    #[serde(default)]
    pub sync: SyncConfig,

    /// GitHub access for the `github` tool
    #[serde(default)]
    pub github: GithubConfig,

    /// Web search / extraction tuning
    #[serde(default)]
    pub web: WebConfig,

    /// Tool router: per-turn subset of the tool schemas (local encoder)
    #[serde(default)]
    pub router: RouterConfig,

    /// External MCP servers Rijo Code consumes (ADR 0017)
    #[serde(default)]
    pub mcp: McpConfig,

    /// Execution sandbox for `terminal`/`execute_code` (ADR 0018)
    #[serde(default)]
    pub sandbox: SandboxConfig,

    /// Rijo Box tool socket (ADR 0011). `RIJO_BOX_ADDR` overrides and enables the toolset.
    #[serde(default, rename = "box")]
    pub box_config: BoxConfig,
}

/// External MCP servers Rijo Code consumes (ADR 0017).
///
/// The agent reaches them through one `mcp` tool. A remote tool has no native
/// guard of its own, so only the names listed in a server's `read_only_tools`
/// run without a dialog; every other call is treated as destructive (TUI
/// confirmation, denied by default headless) — the same rule as `terminal`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfig {
    /// Servers to connect to over stdio. Empty = the `mcp` tool has nothing to talk to.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Unique name — used by the tool and in the approval message.
    pub name: String,
    /// Executable that speaks MCP over stdio.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment for the child process. Values are never logged.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Tool names that may run without the approval dialog. Everything else
    /// counts as a destructive action.
    #[serde(default)]
    pub read_only_tools: Vec<String>,
    /// A configured server is enabled unless explicitly switched off.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl McpConfig {
    /// The enabled server with this name, if any.
    pub fn server(&self, name: &str) -> Option<&McpServerConfig> {
        self.servers.iter().find(|s| s.name == name && s.enabled)
    }
}

impl McpServerConfig {
    /// May this remote tool run without asking the user?
    pub fn is_read_only(&self, tool: &str) -> bool {
        self.read_only_tools.iter().any(|t| t == tool)
    }
}

/// Execution sandbox for `terminal`/`execute_code` (ADR 0018).
///
/// `backend`:
/// - `off` (default) — host execution, unchanged;
/// - `auto` / `bwrap` — wrap the process in bubblewrap on Linux
///   (read-only system, the workspace writable, network optional);
/// - `box` — run inside a Rijo Box (`box.v1 exec.run`). The client is pending,
///   so selecting it fails **closed** — it never silently runs on the host.
///
/// Default is `off` on purpose: turning the wrapper on by default would change
/// every command unmeasured. It is opt-in until the bwrap path has been measured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default = "default_sandbox_backend")]
    pub backend: String,
    /// Allow network inside the sandbox (bwrap: omit `--unshare-net`).
    #[serde(default = "default_true")]
    pub network: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            backend: default_sandbox_backend(),
            network: true,
        }
    }
}

fn default_sandbox_backend() -> String {
    "off".to_string()
}

/// Rijo Box tool socket (ADR 0011).
///
/// `addr` is the `host:port` of a running `rijo-box serve` (P0: TCP loopback). The
/// `RIJO_BOX_ADDR` environment variable overrides it and is what **enables** the `box` toolset —
/// with no address the tools are not registered and cost no prompt weight.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BoxConfig {
    #[serde(default)]
    pub addr: Option<String>,
}

/// Web search and extraction tuning.
///
/// The defaults exist because of a measured problem: when a search provider
/// hangs, every single search used to pay the full client timeout before the
/// fallback answered.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WebConfig {
    /// Timeout for one search request (the built-in DuckDuckGo call).
    #[serde(default = "default_search_timeout_ms")]
    pub search_timeout_ms: u64,
    /// After a failed search, skip the provider for this long (0 = never skip).
    /// DuckDuckGo is scraped and throttles, so the default is short.
    #[serde(default = "default_search_cooldown_secs")]
    pub search_cooldown_secs: u64,
    /// Search cache lifetime in seconds (0 = 24 h default).
    #[serde(default)]
    pub cache_ttl_secs: u64,
    /// How many web_extract URLs are fetched at the same time.
    #[serde(default = "default_extract_parallel")]
    pub extract_parallel: usize,
    /// Override the built-in DuckDuckGo Lite endpoint (tests, mirrors).
    /// `None` = the public `lite.duckduckgo.com` endpoint.
    #[serde(default)]
    pub ddg_api: Option<String>,
    /// Per-provider base-URL overrides for `web_search`, keyed by provider name
    /// (`wikipedia`, `crates-io`, `mdn`, …). Empty = the real endpoints. Tests
    /// and mirrors point these at a local server so no test touches the network.
    #[serde(default)]
    pub endpoints: std::collections::BTreeMap<String, String>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            search_timeout_ms: default_search_timeout_ms(),
            search_cooldown_secs: default_search_cooldown_secs(),
            cache_ttl_secs: 0,
            extract_parallel: default_extract_parallel(),
            ddg_api: None,
            endpoints: std::collections::BTreeMap::new(),
        }
    }
}

fn default_search_timeout_ms() -> u64 {
    10_000
}

/// DuckDuckGo Lite is scraped and throttles after a burst of requests; a short
/// cooldown lets it recover within a session instead of being disabled for
/// 15 minutes.
fn default_search_cooldown_secs() -> u64 {
    60
}

fn default_extract_parallel() -> usize {
    5
}

/// GitHub access for the `github` tool.
///
/// Deliberately without a token field: the token comes from the environment or
/// from `gh auth token`, so a copy of config.yaml never carries credentials.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GithubConfig {
    /// Override the API base (tests point this at a local stub).
    #[serde(default)]
    pub api_base: Option<String>,
    /// Name of the environment variable holding the token, if it is not one of
    /// GH_TOKEN / GITHUB_TOKEN.
    #[serde(default)]
    pub token_env: Option<String>,
    /// Fall back to `gh auth token` when no environment variable is set.
    #[serde(default = "default_true_value")]
    pub use_gh_cli: bool,
    /// How long read results are reused inside a turn (0 disables the cache).
    #[serde(default = "default_github_cache_ttl")]
    pub cache_ttl_secs: u64,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            api_base: None,
            token_env: None,
            use_gh_cli: true,
            cache_ttl_secs: default_github_cache_ttl(),
        }
    }
}

fn default_true_value() -> bool {
    true
}

fn default_github_cache_ttl() -> u64 {
    30
}

/// Sync between machines.
///
/// Default is OFF and the workspace allowlist is empty: the brain index holds
/// source code, so "off unless explicitly allowed" is the only safe default.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncConfig {
    #[serde(default)]
    pub enabled: bool,
    /// "git" (private repo) or "rsync" (script transport, not built yet).
    #[serde(default = "default_sync_transport")]
    pub transport: String,
    /// "owner/name" of the private repo.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default = "default_sync_branch")]
    pub branch: String,
    /// Local checkout of the sync directory; default `~/.rijo/sync`.
    #[serde(default)]
    pub dir: Option<std::path::PathBuf>,
    /// Workspaces whose brain nodes may be exported. Empty = nothing.
    #[serde(default)]
    pub workspaces: Vec<std::path::PathBuf>,
    /// Remote workspace path -> local path, for machines that check the same
    /// repository out somewhere else.
    #[serde(default)]
    pub workspace_map: std::collections::BTreeMap<String, std::path::PathBuf>,
    /// "pull" imports at start, "off" does not.
    #[serde(default = "default_sync_on_start")]
    pub on_start: String,
    /// "push" exports at exit, "off" does not.
    #[serde(default = "default_sync_on_exit")]
    pub on_exit: String,
    /// Background sync every N minutes while Rijo Code runs (0 = off).
    #[serde(default)]
    pub interval_min: u64,
    /// Create the private repo automatically on first start (opt-in: it creates
    /// a GitHub artifact without being asked).
    #[serde(default)]
    pub auto_init: bool,
    /// Sessions travel too (they grow fastest; off by default).
    #[serde(default)]
    pub include_sessions: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: default_sync_transport(),
            repo: None,
            branch: default_sync_branch(),
            dir: None,
            workspaces: Vec::new(),
            workspace_map: std::collections::BTreeMap::new(),
            on_start: default_sync_on_start(),
            on_exit: default_sync_on_exit(),
            interval_min: 0,
            auto_init: false,
            include_sessions: false,
        }
    }
}

fn default_sync_transport() -> String {
    "git".into()
}
fn default_sync_branch() -> String {
    "main".into()
}
fn default_sync_on_start() -> String {
    "pull".into()
}
fn default_sync_on_exit() -> String {
    "push".into()
}

impl LlmConfig {
    /// Context window to display: an explicit `context_window` wins, otherwise
    /// the model name is looked up. `None` = unknown (no percentage shown).
    pub fn effective_context_window(&self) -> Option<u64> {
        if self.context_window > 0 {
            return Some(self.context_window);
        }
        crate::models::known_context_window(&self.model)
    }
}

/// Learning review after the turn — Hermes parity.
///
/// After a turn, a cheap LLM call decides whether the turn yields a
/// durable lesson (`SKILL_CREATE` / `SKILL_UPDATE` / `MEMORY`).
/// The call runs in the background; its cost is recorded as its own
/// `kind: "learn"` entry in `turn_metrics.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningConfig {
    /// Start a review after every turn (on by default).
    #[serde(default = "default_true")]
    pub after_turn: bool,
    /// Review only from this many loop iterations on — small turns are not worth it.
    #[serde(default = "default_min_iterations")]
    pub min_iterations: u32,
    /// Maximum number of write actions per review.
    #[serde(default = "default_max_actions")]
    pub max_actions: usize,
    /// Output cap for the review call (tokens).
    #[serde(default = "default_learn_max_tokens")]
    pub max_tokens: u32,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            after_turn: true,
            min_iterations: default_min_iterations(),
            max_actions: default_max_actions(),
            max_tokens: default_learn_max_tokens(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_min_iterations() -> u32 {
    2
}
fn default_max_actions() -> usize {
    3
}
fn default_learn_max_tokens() -> u32 {
    800
}

/// Compiler/lint feedback after changes to source files.
///
/// OFF by default, because the cost depends on the project: `cargo check`
/// takes minutes on a cold `target/`, 1–3 s per change on a warm one.
/// The benefit is one saved iteration per error (a median iteration costs
/// ~1.2 s of LLM round trip plus tool latency).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsConfig {
    /// After write_file/patch on a source file, run the check command
    /// and append its output to the tool result.
    #[serde(default)]
    pub after_edit: bool,
    /// Custom check command; `{file}` is replaced by the changed path.
    /// If unset, it is detected from the project (Cargo.toml → cargo check,
    /// tsconfig.json → tsc, .py → py_compile).
    #[serde(default)]
    pub command: Option<String>,
    /// Hard upper limit; after that the process is killed and this is reported.
    #[serde(default = "default_diag_timeout_ms")]
    pub timeout_ms: u64,
    /// This many output lines go to the model (the rest is marked as truncated).
    #[serde(default = "default_diag_max_lines")]
    pub max_lines: usize,
}

fn default_diag_timeout_ms() -> u64 {
    30_000
}

fn default_diag_max_lines() -> usize {
    40
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            after_edit: false,
            command: None,
            timeout_ms: default_diag_timeout_ms(),
            max_lines: default_diag_max_lines(),
        }
    }
}

/// Limits token growth in the loop. Without it, the history grows without
/// bound per turn (tool output is the biggest item) and long sessions run
/// into context overflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    /// Max characters of ONE tool result that go into the history. Larger
    /// outputs are truncated; the full text is spilled to a file and the path
    /// is passed to the model (it can use it via read_file/search_files).
    #[serde(default = "default_tool_result_cap")]
    pub tool_result_cap: usize,
    /// Tool results older than the last N messages are stubbed down to
    /// one-liners (the structure stays valid: assistant.tool_calls + tool pairs).
    #[serde(default = "default_tool_stub_after")]
    pub tool_stub_after_messages: usize,
    /// Hard upper bound on messages in the request. Older units are
    /// dropped; the first user message (the task) is kept.
    #[serde(default = "default_max_messages")]
    pub max_messages: usize,
    /// Estimated token budget for the request (≈4 chars/token). When the history
    /// exceeds it, the oldest assistant/tool units are dropped — a cheap
    /// per-iteration backstop so a long task stays inside the window between the
    /// (expensive) LLM compressions. Count-based pruning alone misses a few huge
    /// tool results. 0 = off.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    /// Compress the older history once the fill reaches this share of the model
    /// window (percent).
    #[serde(default = "default_compress_at_pct")]
    pub compress_at_pct: u32,
    /// Run the compression automatically (off = only when asked).
    #[serde(default = "default_true_value")]
    pub auto_compress: bool,
    /// Budget of one compression block, in tokens.
    #[serde(default = "default_summary_max_tokens")]
    pub summary_max_tokens: u32,
    /// Messages at the end of the history that stay verbatim.
    #[serde(default = "default_keep_recent_messages")]
    pub keep_recent_messages: usize,
    /// Do not compress when it would save less than this (the prompt cache is
    /// invalidated by any change to the prefix).
    #[serde(default = "default_min_saving_tokens")]
    pub min_saving_tokens: u32,
    /// Minimum number of turns between two compressions.
    #[serde(default = "default_min_turns_between")]
    pub min_turns_between: u32,
    /// Hard cap on LLM iterations per turn. On reaching it the loop asks the
    /// model once for a final answer instead of returning a bare error — a broad
    /// task must still end with the user seeing a result.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
}

fn default_tool_result_cap() -> usize {
    6_000
}
fn default_tool_stub_after() -> usize {
    6
}
fn default_max_messages() -> usize {
    80
}
fn default_max_tokens() -> usize {
    0
}

fn default_compress_at_pct() -> u32 {
    80
}

fn default_summary_max_tokens() -> u32 {
    1200
}

fn default_keep_recent_messages() -> usize {
    12
}

fn default_min_saving_tokens() -> u32 {
    2000
}

fn default_min_turns_between() -> u32 {
    5
}

fn default_max_iterations() -> usize {
    25
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            tool_result_cap: 6_000,
            tool_stub_after_messages: 6,
            max_messages: 80,
            max_tokens: default_max_tokens(),
            compress_at_pct: default_compress_at_pct(),
            auto_compress: true,
            summary_max_tokens: default_summary_max_tokens(),
            keep_recent_messages: default_keep_recent_messages(),
            min_saving_tokens: default_min_saving_tokens(),
            min_turns_between: default_min_turns_between(),
            max_iterations: default_max_iterations(),
        }
    }
}

/// Tool router: which tool schemas this turn actually needs (a subset of the
/// registry), decided by a local encoder (ONNX) before the LLM call.
///
/// Why: the tool schemas are ~10k characters — the largest fixed part of the
/// prompt per turn. A small encoder picks the relevant ones; the full set is
/// the fallback.
///
/// Off by default: it switches on only after the A/B gate (--schema-eval,
/// full vs. router) has shown that the tool choice does not get worse. Until
/// then the decision trace is written anyway — that is the live training
/// data for the retrain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    /// Filter the tool schemas per turn. A missing/broken model bundle is
    /// never an error: the turn then falls back to the full tool set.
    #[serde(default)]
    pub enabled: bool,
    /// Model bundle under `<models>/router/<version>`.
    #[serde(default = "default_router_model")]
    pub model: String,
    /// Below this score a tool is not sent (per-class thresholds from
    /// thresholds.json apply first; this is the global floor).
    #[serde(default = "default_router_min_score")]
    pub min_score: f32,
    /// Hard cap on the predicted tools per turn (after the thresholds). The
    /// bundle's own `max_tools` is the fallback; always-keep tools are added
    /// after the cap. `0` keeps the bundle value.
    #[serde(default = "default_router_max_tools")]
    pub max_tools: usize,
    /// Which heads are active: tools (the filter), guard (advisory second
    /// opinion on destructive commands), compact (keep/stub/drop suggestions).
    /// Guard and compact stay off until their own gate (E0b) has passed.
    #[serde(default)]
    pub heads: RouterHeads,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterHeads {
    /// The tool-subset head — the reason the router exists.
    #[serde(default = "default_true")]
    pub tools: bool,
    /// Advisory only: never overrides the regex guard.
    #[serde(default)]
    pub guard: bool,
    /// Suggestion for prune_context (the hard message cap stays).
    #[serde(default)]
    pub compact: bool,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: default_router_model(),
            min_score: default_router_min_score(),
            max_tools: default_router_max_tools(),
            heads: RouterHeads::default(),
        }
    }
}

impl Default for RouterHeads {
    fn default() -> Self {
        Self {
            tools: true,
            guard: false,
            compact: false,
        }
    }
}

fn default_router_model() -> String {
    "v1".into()
}
fn default_router_min_score() -> f32 {
    0.25
}
fn default_router_max_tools() -> usize {
    6
}

/// TTS configuration — deliberately local engines only (piper, espeak-ng, espeak).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TtsConfig {
    /// Voice: empty = backend default (espeak-ng/espeak: "de"; piper: model voice).
    /// Examples: "de", "de+f3", "en-us".
    #[serde(default)]
    pub voice: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// API base URL (OpenAI-compatible)
    pub base_url: String,
    /// API key
    pub api_key: String,
    /// Model name
    pub model: String,
    /// Max tokens for output
    pub max_tokens: u32,
    /// Context window of the model in tokens — used ONLY for the status-line
    /// gauge. `0` (the default) means AUTO: the window is detected from the model
    /// name (`crate::models::known_context_window`). Set a number only to
    /// override a wrong or unknown detection — the gauge then shows the prompt
    /// size without a percentage rather than inventing a denominator.
    #[serde(default)]
    pub context_window: u64,
    /// Temperature
    pub temperature: f32,
    /// Reasoning effort for thinking models: "min"|"med"|"max" (default: med).
    /// Research-backed: medium hits the accuracy/latency knee — high effort took
    /// 39.4s vs 4.1s (medium) for identical reasoning quality (live probe 2026-09-06).
    #[serde(default = "default_thinking_effort")]
    pub thinking_effort: String,
    /// OpenCode-style APIs (https://opencode.ai/zen/.../v1) reject requests
    /// without a session header: "MissingSessionID … cannot be routed
    /// efficiently". Set this to the header name (usually
    /// `x-opencode-session`) when using such a provider; Rijo Code generates one
    /// id per process and sends it. Empty = no extra header.
    #[serde(default)]
    pub session_header: String,
    /// Extra HTTP headers as `Name: value` lines. Credentials do NOT belong
    /// here — the token comes from `api_key` (config) or `RIJO_API_KEY`
    /// (environment), so a copied config file never carries a secret.
    #[serde(default)]
    pub extra_headers: Vec<String>,
}

fn default_thinking_effort() -> String {
    "med".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Embedding model for vector DB
    pub embedding_model: String,
    /// Dimension of embedding vectors
    pub embedding_dim: usize,
    /// Brain on/off. Off means: no indexing and no context injection — the
    /// switch exists so the effect can be measured on real tasks (A/B).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Only use nodes from the current workspace root. Prevents pointers into
    /// other projects (measured before: 8 of 74 sources did not exist here).
    #[serde(default = "default_true")]
    pub workspace_only: bool,
    /// How many hits are injected into the system prompt.
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    /// Cosine floor for vector hits. Below it nothing is injected: an
    /// irrelevant pointer costs tokens and misleads. Full-text hits are not
    /// filtered by it (no comparable score).
    #[serde(default = "default_min_score")]
    pub min_score: f64,
}

fn default_top_k() -> usize {
    3
}
fn default_min_score() -> f64 {
    0.30
}

/// Serde default for `skill_roots`: also read the Hermes library, even if the
/// existing config.yaml does not know the key (otherwise [] = Rijo Code only).
fn default_skill_roots() -> Vec<PathBuf> {
    vec![crate::paths::hermes_skills_dir()]
}

impl Default for Config {
    fn default() -> Self {
        let rijo_home = crate::paths::rijo_home();

        Self {
            llm: LlmConfig {
                base_url: "https://api.openai.com/v1".into(),
                api_key: std::env::var("RIJO_API_KEY").unwrap_or_default(),
                model: "gpt-4o-mini".into(),
                max_tokens: 4096,
                temperature: 0.7,
                thinking_effort: "med".into(),
                context_window: 0,
                session_header: String::new(),
                extra_headers: Vec::new(),
            },
            memory: MemoryConfig {
                embedding_model: "all-MiniLM-L6-v2".into(),
                embedding_dim: 384,
                enabled: true,
                workspace_only: true,
                top_k: default_top_k(),
                min_score: default_min_score(),
            },
            skills_dir: rijo_home.join("skills"),
            // Also read the Hermes library: Rijo Code should see the same skills
            // as Hermes (~/.hermes/skills), not just its own.
            skill_roots: vec![crate::paths::hermes_skills_dir()],
            workspace: PathBuf::from("."),
            data_dir: rijo_home.join("data"),
            terminal_timeout: 180,
            max_concurrent_tools: 16,
            browser_backend: "none".into(),
            tts: TtsConfig::default(),
            context: ContextConfig::default(),
            learning: LearningConfig::default(),
            sync: SyncConfig::default(),
            github: GithubConfig::default(),
            web: WebConfig::default(),
            diagnostics: DiagnosticsConfig::default(),
            router: RouterConfig::default(),
            mcp: McpConfig::default(),
            sandbox: SandboxConfig::default(),
            box_config: BoxConfig::default(),
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        // `RIJO_CONFIG` points a run at a different profile — needed to try a
        // first-run without touching the real config, and useful for portable
        // setups. Empty values are ignored.
        if let Ok(path) = std::env::var("RIJO_CONFIG") {
            if !path.trim().is_empty() {
                return PathBuf::from(path.trim());
            }
        }
        crate::paths::rijo_home().join("config.yaml")
    }

    /// Load config from file or create default.
    /// Without an explicit path: ~/.rijo/config.yaml, else defaults.
    pub fn load(path: Option<PathBuf>) -> Self {
        let p = path.unwrap_or_else(Self::default_path);
        let mut cfg = if p.exists() {
            match std::fs::read_to_string(&p) {
                Ok(content) => match serde_yaml::from_str::<Config>(&content) {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        tracing::warn!("bad config {}: {e} — using defaults", p.display());
                        Self::default()
                    }
                },
                Err(_) => Self::default(),
            }
        } else {
            Self::default()
        };
        // Expand '~' in path fields (yaml may contain literal tilde)
        let expand = |s: &PathBuf| crate::paths::expand_tilde(s);
        cfg.skills_dir = expand(&cfg.skills_dir);
        cfg.data_dir = expand(&cfg.data_dir);
        cfg.skill_roots = cfg.skill_roots.iter().map(&expand).collect();
        cfg.skill_roots.retain(|p| p.exists());
        if !cfg.skill_roots.contains(&cfg.skills_dir) {
            cfg.skill_roots.push(cfg.skills_dir.clone());
        }
        if cfg.workspace.to_str() == Some(".") && std::env::current_dir().is_ok() {
            cfg.workspace = std::env::current_dir().unwrap();
        }
        // Auto max_tokens: if the configured value is below the model default
        // → raise it to the model output hard cap. Otherwise large write_file
        // arguments (whole source files inside the tool call) are cut off.
        cfg.llm.max_tokens = effective_max_tokens(&cfg.llm.model, cfg.llm.max_tokens);
        cfg
    }
}

/// Per-model output-token ceilings (provider-documented hard caps / sensible
/// defaults). The config value may set a floor, but never UNDER the model's
/// useful working limit — agents write whole source files inside one tool call.
fn effective_max_tokens(model: &str, configured: u32) -> u32 {
    let m = model.to_lowercase();
    // (min_needed, hard_cap): high working values, no overkill
    let (floor, cap): (u32, u32) = if m.contains("deepseek-v4") || m.contains("deepseek-v3") {
        (16384, 16384) // DeepInfra hard cap 16384 output
    } else if m.contains("gpt-4") || m.contains("gpt-5") || m.contains("o1") || m.contains("o3") {
        (16384, 32768)
    } else if m.contains("claude") {
        (8192, 64000)
    } else {
        // deepseek (non v3/v4), glm, llama, mistral, qwen and unknown models:
        // the generic working floor/cap.
        (8192, 16384)
    };
    if configured < floor {
        floor
    } else if configured > cap {
        cap
    } else {
        configured
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An existing config.yaml without a `router:` section must keep working
    /// and come out with the safe defaults (off, fallback = full tool set).
    #[test]
    fn router_defaults_when_section_missing() {
        let cfg: Config = serde_yaml::from_str(
            "llm:\n  base_url: http://x\n  api_key: k\n  model: m\n  max_tokens: 4096\n  temperature: 0.7\n\
             memory:\n  embedding_model: e\n  embedding_dim: 384\n  max_entries: 500\n  similarity_threshold: 0.85\n\
             skills_dir: /tmp/s\nworkspace: /tmp/w\ndata_dir: /tmp/d\nterminal_timeout: 180\n\
             max_concurrent_tools: 16\nbrowser_backend: none\n",
        )
        .expect("config without router section parses");
        assert!(!cfg.router.enabled, "router must be off by default");
        assert_eq!(cfg.router.model, "v1");
        assert_eq!(cfg.router.max_tools, 6);
        assert!((cfg.router.min_score - 0.25).abs() < f32::EPSILON);
        assert!(cfg.router.heads.tools, "tools head on by default");
        assert!(!cfg.router.heads.guard, "guard head off until gate E0b");
        assert!(!cfg.router.heads.compact, "compact head off until gate E0b");
    }

    /// Explicit section wins, including the nested heads flags.
    #[test]
    fn router_section_overrides() {
        let cfg: Config = serde_yaml::from_str(
            "llm:\n  base_url: http://x\n  api_key: k\n  model: m\n  max_tokens: 4096\n  temperature: 0.7\n\
             memory:\n  embedding_model: e\n  embedding_dim: 384\n  max_entries: 500\n  similarity_threshold: 0.85\n\
             skills_dir: /tmp/s\nworkspace: /tmp/w\ndata_dir: /tmp/d\nterminal_timeout: 180\n\
             max_concurrent_tools: 16\nbrowser_backend: none\n\
             router:\n  enabled: true\n  max_tools: 5\n  heads:\n    guard: true\n",
        )
        .expect("config with router section parses");
        assert!(cfg.router.enabled);
        assert_eq!(cfg.router.max_tools, 5);
        assert!(cfg.router.heads.guard);
        assert!(
            !cfg.router.heads.compact,
            "unspecified head keeps its default"
        );
    }
}
