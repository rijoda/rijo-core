# Rijo Code — native Rust agent harness

> **Historical quickstart — superseded.** The canonical entry point is the
> repository-root [`README.md`](../README.md); architecture lives in
> [`ARCHITECTURE.md`](../ARCHITECTURE.md). Parts of this page are no longer
> accurate (`skills.rs` was removed, `brain` is now a module directory, and the
> TUI minimap is a parked, not-yet-wired feature). Kept as an early record.

Blazing-fast agentic coding harness. Hermes-parity tools in Rust, no tool-call bottleneck.

## Quickstart

```bash
# Build
cargo build --release

# Run
rijo

# First setup
rijo --help
```

## Architecture

```
crates/
  rijo-core/         # Shared library (tools, config, executor, TUI)
    src/
      tools/          # Tool implementations (one file per tool)
      executor.rs     # Agent loop: LLM calls, tool dispatch, progress
      tui.rs          # Ratatui terminal UI
      watcher.rs      # FS watcher for hot-reload
      llm.rs          # LLM API client (OpenAI-compatible)
      config.rs       # YAML config loader
      trust.rs        # Env key isolation + workspace trust
      selfupd.rs      # Binary self-update via staging
      skills.rs       # Skills management (directory-based)
      brain.rs        # Embedding-based memory (unwired)
      bench.rs        # Benchmarking infrastructure
      cli.rs          # CLI argument parsing
  rijo-cli/         # Thin binary crate (subsecond hot-patch entry)
    src/main.rs
```

## Features

| Feature | Status | Description |
|---------|--------|-------------|
| Tools (read, write, patch, search, terminal, skills, memory, …) | ✅ | Async + sync, Hermes-compatible |
| TUI (Ratatui) | ✅ | Scrollable chat, minimap, input, streaming |
| Hot-reload (config.yaml) | ✅ | Watcher-based, debounced 800ms |
| Hot-reload (skills/) | ✅ | Content-aware (mtime), no false positives |
| Binary self-update | ✅ | Staging dir + graceful restart |
| Subsecond integration | ✅ | Thin-link patching |
| Shell into TUI on start | ❌ | Planned |
| Delete file (safe) | ✅ | Workspace-scoped, per-delete confirmation |
| .env key isolation | ✅ | Sealed arena, never exposed to LLM |
| Trust prompt | ✅ | First-run workspace confirmation |

## Configuration

File: `~/.rijo/config.yaml`

```yaml
llm:
  base_url: https://api.openai.com/v1
  api_key: "${RIJO_API_KEY}"
  model: gpt-4o-mini
  max_tokens: 4096
  temperature: 0.7
```

## Keybindings

| Key | Action |
|-----|--------|
| Enter | Send message |
| Alt/Shift/Ctrl+Enter | New line inside the prompt (the input box grows to 8 lines, then scrolls) |
| ↑/↓ | Multi-line prompt: move the cursor between lines. Empty input: scroll the chat. Otherwise: input history |
| PgUp/PgDn | Scroll chat |
| Ctrl+T | Expand the todo strip into the full list |
| Question overlay: ↑/↓ or 1-9 | Pick an option |
| Question overlay: Enter | Confirm and move to the next question |
| Question overlay: Tab / Esc | Switch question / skip (answers what is selected) |
| Tab / Shift+Tab | Jump between prompts |
| Mouse wheel | Scroll |
| Ctrl+C | Cancel / Quit |
| Ctrl+P | Command palette |
| @ | File picker |
| /command | Slash commands |
