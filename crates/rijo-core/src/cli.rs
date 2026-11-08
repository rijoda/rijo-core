//! CLI entry point

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Optional subcommands. Everything else stays a flag on the main command.
#[derive(Subcommand, Clone, Debug)]
pub enum Command {
    /// Model Context Protocol: expose the native tools to other agents.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
}

#[derive(Subcommand, Clone, Debug)]
pub enum McpAction {
    /// Serve MCP over stdio until the client disconnects.
    Serve,
    /// List the configured MCP servers, or one server's tools.
    List {
        /// Server name; when given, its tools are listed (opens a connection).
        server: Option<String>,
    },
}

#[derive(Parser)]
#[command(name = "rijo", version, about = "Blazing-fast agentic coding harness")]
pub struct Cli {
    /// Optional subcommand, e.g. `rijo mcp serve`.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Config file path
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Run a single query
    #[arg(short, long)]
    pub query: Option<String>,

    /// Child mode for delegate_task: --child <brief-file> <result-file>
    #[arg(long = "child", value_names = ["brief_file", "result_file"])]
    pub child: Option<Vec<String>>,

    /// Run benchmark
    #[arg(long)]
    pub bench: bool,

    /// Write the portable sync directory (brain nodes, skills, memory)
    #[arg(long = "sync-export")]
    pub sync_export: bool,

    /// Merge a sync directory into this machine's state
    #[arg(long = "sync-import")]
    pub sync_import: bool,

    /// Show the sync configuration and the state of the last export
    #[arg(long = "sync-status")]
    pub sync_status: bool,

    /// Download the embedding model and tokenizer ONCE into the model cache
    /// (~/.rijo/models) — the install-time step; every later start reuses it.
    #[arg(long = "download-models")]
    pub download_models: bool,

    /// Index the workspace into the brain and exit (no model call).
    /// Prints the summary; useful before one-shot runs, which skip indexing.
    #[arg(long)]
    pub index: bool,

    /// Run E2E test (identical webapp prompt from data/e2e_prompt.json)
    #[arg(long)]
    pub e2e: bool,

    /// List registered tools
    #[arg(long)]
    pub tools: bool,

    /// Internal benchmark worker: read `{"tool","args"}` JSON lines from stdin,
    /// dispatch one tool per line, and print one timing JSON line per call.
    /// The process stays warm, so the harness measures dispatch, not startup.
    #[arg(long = "bench-stdio")]
    pub bench_stdio: bool,

    /// Resume the most recent session (loads history into the TUI)
    #[arg(long)]
    pub resume: bool,

    /// Headless scripted session: read prompts from stdin (one per line), run
    /// one turn each, all in ONE process. Keeps the provider's prompt cache
    /// across turns — used for batch runs and on/off measurements.
    #[arg(long)]
    pub repl: bool,

    /// Schema evaluation: shorten tool descriptions and measure quality
    /// (real model, task catalog data/schema_tasks.json)
    #[arg(long = "schema-eval")]
    pub schema_eval: bool,

    /// Shortening variants, comma-separated:
    /// full, no_emoji, first_sentence, no_param_desc, compact, minimal
    #[arg(long, default_value = "full,no_emoji,compact")]
    pub variants: String,

    /// Repeats per task (more = more stable, more expensive)
    #[arg(long, default_value_t = 2)]
    pub repeats: usize,

    /// Only the first N tasks (faster pass)
    #[arg(long)]
    pub limit: Option<usize>,

    /// With --schema-eval: subset the tools per task with the router
    /// (gate E2 — the A/B is `--variants full` with and without this flag)
    #[arg(long = "router-eval")]
    pub router_eval: bool,

    /// Shows the TUI views as text (idle, running, overlays) —
    /// check the look without a real terminal. Optional with --width/--height.
    #[arg(long = "tui-snapshot")]
    pub tui_snapshot: bool,

    /// Width of the TUI snapshot (default 100)
    #[arg(long, default_value_t = 100)]
    pub width: u16,

    /// Height of the TUI snapshot (default 24)
    #[arg(long, default_value_t = 24)]
    pub height: u16,

    /// Shows the character breakdown of the tool schemas per tool
    /// (without an API call) — shows where shortening pays off most
    #[arg(long = "schema-report")]
    pub schema_report: bool,

    /// Write the router label tables (tool name → toolset, in the exact
    /// neuron order) to <data_dir>/router/labels_registry.json and exit.
    /// The dataset exporter reads this file — a renamed tool then shows up
    /// as a mismatch instead of training against a stale list.
    #[arg(long = "dump-labels")]
    pub dump_labels: bool,

    /// Use a known provider template for this run, e.g. `opencode` (Zen/Go,
    /// including the free development models). Sets base URL and the headers
    /// the API requires; the token is taken from the provider's own
    /// environment variable or its credential file, never copied into a config.
    #[arg(long)]
    pub provider: Option<String>,

    /// Override the model for this run (e.g. `space-bunny-free`).
    #[arg(long)]
    pub model: Option<String>,

    /// List the provider templates, their base URLs and free models, and
    /// whether a token is available. No API call.
    #[arg(long = "list-providers")]
    pub list_providers: bool,

    /// Base URL override for this run (any OpenAI-compatible endpoint).
    #[arg(long = "base-url")]
    pub base_url: Option<String>,

    /// Run a GGUF model locally: start (and own) a persistent `llama-server` for
    /// this run and point the client at it. The server is stopped when Rijo Code
    /// exits; because it stays alive, llama-server's prefix KV cache survives
    /// across turns (measured: 33x less prefill warm than cold).
    #[arg(long, value_name = "MODEL.gguf")]
    pub local: Option<String>,

    /// Path to the `llama-server` binary used by `--local` (default: search PATH).
    #[arg(long = "local-server", value_name = "PATH")]
    pub local_server: Option<String>,

    /// Port for the server started by `--local`.
    #[arg(long = "local-port", default_value_t = 8080)]
    pub local_port: u16,
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse()
    }
}

/// Prompt the user on stderr to approve a destructive action.
/// Returns true if the user typed 'y'/'yes'/'j'/'ja'.
pub fn prompt_dangerous(message: &str) -> bool {
    use std::io::Write;
    eprintln!("\n⚠  {}", message);
    eprint!("   Confirm with 'y' / N to abort: ");
    let _ = std::io::stderr().flush();
    let mut input = String::new();
    match std::io::stdin().read_line(&mut input) {
        Ok(_) => matches!(
            input.trim().to_lowercase().as_str(),
            "y" | "yes" | "j" | "ja"
        ),
        Err(_) => false,
    }
}
