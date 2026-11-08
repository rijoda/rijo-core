# Rijo Core

[![CI](https://github.com/rijoda/rijo-core/actions/workflows/ci.yml/badge.svg)](https://github.com/rijoda/rijo-core/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](#license)

**The open engine behind Rijo Code** — a local-first agentic coding harness in Rust,
built by [Rijoda Labs](https://github.com/rijoda).

Rijo Core is the part you can read, build and extend: the turn loop, **34 native tools**,
the safety guard, sessions, the workspace brain, the tool router, the LLM adapter and a
frozen terminal frontend. The **Rijo Code desktop workbench** is a separate, closed
application that consumes only the engine's public surface.

**Status:** `0.1.0`, early. Linux-first; the Windows CLI path is tested.
The engine has **no terminal or GUI dependency** — that is a property of the build, not a
promise.

---

## Why it exists

- **Every tool runs natively in Rust.** No interpreter start-up per call, no second tool
  implementation behind a UI.
- **One loop and one registry.** The tool catalog is the single source of truth; names stay
  sorted so the provider's prompt cache stays effective.
- **The guard covers every execution path** — `terminal`, `execute_code`, script contents,
  MCP and box crossings. It is documented as a heuristic, not a sandbox.
- **A frontend never reimplements core knowledge.** It calls `inspect` and receives
  `UiEvent`s.

---

## What is in this repository

| Crate | Role |
|---|---|
| `crates/rijo-core` | executor, 34 native tools, guard, sessions, transcript model, `ui_events`/`inspect`, LLM adapter, brain, router, config |
| `crates/rijo-cli` | the binary **`rijo`** — headless runs, REPL, MCP server and client |
| `crates/rijo-tui` | the frozen terminal frontend; owns `crossterm`, `ratatui`, clipboard input |

The closed desktop app is **not** in this repository. Boundary and rationale:
[`docs/STRUCTURE.md`](docs/STRUCTURE.md),
[`docs/features/core-ui-boundary.md`](docs/features/core-ui-boundary.md), and
[`docs/decisions/0001-open-core-closed-workbench.md`](docs/decisions/0001-open-core-closed-workbench.md).

---

## Quick start

### Build from source

```bash
cargo build --release -p rijo
./target/release/rijo --tools    # list the 34 tools
./target/release/rijo            # interactive terminal UI
```

Requirements: Rust **≥ 1.88**, `git`, and either a provider API key or a local
OpenAI-compatible endpoint. On Windows the ONNX Runtime is vendored; on Linux the `ort`
build fetches it once.

### Install

```bash
./install.sh                  # Linux: ~/.local/bin/rijo (no sudo)
```

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1
# Windows: %LOCALAPPDATA%\Programs\rijo
```

Both scripts build from this checkout. `install.sh --binary <tag>` downloads a published
release asset instead; assets exist once a `v*` tag has been released.

### First run and providers

The first start shows a short setup (provider, base URL, model, key) and writes
`~/.rijo/config.yaml` (mode 600 on Unix). Any OpenAI-compatible endpoint works:

```bash
rijo --provider deepseek
rijo --provider opencode
rijo --base-url http://127.0.0.1:8080/v1 --model qwen3-1.7b
rijo --local ./model.gguf        # own a persistent llama-server for this run
rijo --list-providers            # built-in templates, URLs and key variables
```

`RIJO_API_KEY` overrides the key. `--model` overrides the model for one run. A key is
never written into the config by these flags.

### Headless and MCP

```bash
rijo --query "..."               # one turn, no TUI
printf '...\n...\n' | rijo --repl  # one turn per stdin line, one process
rijo --tools                     # registered tools
rijo --bench                     # tool-latency benchmark
```

```bash
rijo mcp serve                   # expose the 34 tools to an MCP client over stdio
rijo mcp list                    # configured external MCP servers
rijo mcp list <server>           # one server's tools
```

`mcp serve` exposes two meta-tools (`list_tools`, `call_tool`) over the same registry, so
the MCP surface and the CLI can never drift apart.

### Where state lives

```
~/.rijo/config.yaml        provider, model and workspace settings (mode 600)
~/.rijo/data/              sessions.db, traces, metrics, process logs
~/.rijo/skills/            own and learned skills
~/.rijo/models/fastembed/  embedding model (downloaded once)
```

`RIJO_CONFIG` points a run at another config file. The data directory defaults to
`~/.rijo/data` and can be changed in `config.yaml`.

---

## The 34 native tools

| Toolset | Tools |
|---|---|
| **file** | `read_file` `write_file` `patch` `search_files` `delete_file` |
| **terminal** | `terminal` `process_manage` |
| **git** | `git` |
| **web** | `web_search` `web_extract` |
| **browser** | `browser_navigate` `browser_snapshot` `browser_click` `browser_type` `browser_scroll` `browser_dev` |
| **research** | `arxiv` |
| **brain** | `brain_search` `brain_outline` |
| **skills** | `skills_list` `skill_view` `skill_manage` |
| **github** | `github` |
| **integration** | `mcp` |
| **core** | `clarify` `context` `cronjob_manage` `delegate_task` `execute_code` `memory` `session_search` `text_to_speech` `todo_list` `vision_analyze` |

A config-gated `box` toolset is registered only when `RIJO_BOX_ADDR` is set, so it costs
no prompt weight otherwise.

---

## How it works

```
rijo (CLI/TUI) ──► rijo-core: executor ──► LLM (streaming) ──► tool registry ──► guard
                                        └──► brain (SQLite + embeddings)
```

- **One turn loop**: context assembly → optional tool router → streaming LLM call →
  parallel read-only tool batch → guards → persistence.
- **The guard** routes destructive commands and script contents to a user decision;
  headless runs fail closed. Every decision goes to `guard_trace.jsonl`.
- **The brain** is SQLite + fastembed with hybrid vector/FTS5 retrieval, incremental and
  workspace-scoped. Per turn the top hits are injected as pointers into the prompt.
- **The tool registry** is the single source of truth. Names are sorted so the prompt
  prefix is byte-stable across calls.
- **MCP works in both directions**: the server exposes the registry; the `mcp` tool
  consumes external stdio servers, with approval for anything not declared read-only.

More detail: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

---

## Measured, not estimated

| Claim | Value | Evidence |
|---|---|---|
| Tool latency vs. the Python reference | **188×–1037× faster** | [`docs/status.md`](docs/status.md) |
| Parallel read-only tool batches | **3.83× local, 3.69× network** | [`docs/status.md`](docs/status.md) |
| Provider prompt-cache hit rate | **26 % → 94 %** (sorted tool schemas) | [`docs/status.md`](docs/status.md) |
| Brain warm start | **65 s → 18 ms** (incremental indexing) | [`docs/status.md`](docs/status.md) |
| Router (tool-schema selection) | **−94 % schema characters**; tool choice 79 % → 90 % | [`docs/status.md`](docs/status.md) |

All numbers were measured on real runs; the caveats and the original measurement records
are in [`docs/status.md`](docs/status.md) and [`docs/history/`](docs/history/README.md).

---

## Safety and honest limits

- **The guard is a text heuristic, not a sandbox.** An interpreter switch, a dropped
  script, base64 or runtime assembly gets past it. The real boundary is the Rijo Box.
- **The brain has no graph layer.** The `edges` table exists; nothing writes to it.
- **macOS is untested.** Linux is the tested platform; the Windows CLI path is tested.
- **The TUI is frozen.** Bug fixes only; it will be rebuilt after the desktop covers the
  remaining workflows.
- **CI** runs the same workflow on Linux and Windows. Local runs remain the source of truth
  for the performance numbers.

Read [`SECURITY.md`](SECURITY.md) before running untrusted commands.

---

## Repository layout

```
Cargo.toml                 workspace: rijo-core + rijo-cli + rijo-tui
crates/
  rijo-core/               engine: executor, tools, guard, sessions, brain, router, llm
  rijo-cli/                binary `rijo`: headless, REPL, MCP
  rijo-tui/                frozen terminal frontend
vendor/onnxruntime/        third-party runtime (MIT), used by the brain and router
docs/                      architecture, status, decisions, research, history
data/                      offline fixtures for tests and benchmarks
scripts/                   measurement and router-training scripts
```

The closed desktop workbench lives in a separate private repository and is not part of
this tree.

---

## Documentation

| I need … | Read |
|---|---|
| The architecture and feature map | [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) |
| The open / closed boundary | [`docs/STRUCTURE.md`](docs/STRUCTURE.md) |
| What is proven and what is open | [`docs/status.md`](docs/status.md) |
| One feature in depth | [`docs/features/`](docs/features/index.md) |
| MCP server and client | [`docs/MCP.md`](docs/MCP.md) |
| Install, release and troubleshooting | [`docs/INSTALL.md`](docs/INSTALL.md), [`docs/RELEASE.md`](docs/RELEASE.md) |
| History, measurements, old TUI docs | [`docs/history/`](docs/history/README.md) |

---

## Contributing

- Read [`CONTRIBUTING.md`](CONTRIBUTING.md) and the [`DCO`](DCO).
- Keep the tool registry sorted and the guard on every execution path.
- Run before a PR:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p rijo-core -p rijo-tui
cargo deny check advisories bans licenses
```

- Network-dependent tests are `#[ignore]`d (live web, Chromium, the embedding-model
  download). Run them explicitly with `--ignored` on a machine with network or a warm
  cache.

---

## License

Licensed under the **Apache License, Version 2.0** ([`LICENSE-APACHE`](LICENSE-APACHE)).

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this project shall be licensed under the same terms, without additional
conditions.

**Rijoda**, **Rijoda Labs** and **Rijo** are trademarks of Rijoda Labs. See
[`TRADEMARKS.md`](TRADEMARKS.md).
