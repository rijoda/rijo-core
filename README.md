# Rijo Core

**by Rijoda Labs** — the open engine behind Rijo Code: a local-first agentic coding
harness in Rust.

**Status:** early (v0.1.0). Linux-first; the Windows path works and is tested locally.

---

## What is in this repository

| Crate | Role |
|---|---|
| `crates/rijo-core` | **Rijo Core** — executor, 34 native tools, guard, sessions, transcript model, UI event contract (`ui_events`, `inspect`), LLM adapter, brain, config |
| `crates/rijo-cli` | binary **`rijo`** — headless runs, MCP server and client |
| `crates/rijo-tui` | the terminal frontend (frozen) — consumes `rijo_core::ui_events` |

The **Rijo Code desktop workbench** is a separate, closed application. It consumes only the
public surface of `rijo-core` (`ui_events`, `inspect`, `executor`, `tools`, `config`, `llm`)
and is **not** part of this repository. The boundary rule:
[`docs/features/core-ui-boundary.md`](docs/features/core-ui-boundary.md). Decision:
[`docs/decisions/0001-open-core-closed-workbench.md`](docs/decisions/0001-open-core-closed-workbench.md).

`rijo-core` has no terminal or GUI dependency. That is a property of the build, not a
promise: `cargo check -p rijo-core` succeeds without any frontend.

**The TUI is frozen.** Bug fixes only; it will be rebuilt after the desktop covers the
remaining workflows.

---

## Quick start

```bash
cargo build --release -p rijo
./target/release/rijo            # interactive (terminal UI)
./target/release/rijo --tools    # list all registered tools
./target/release/rijo mcp serve  # expose the tools to an MCP client over stdio
```

Set `RIJO_API_KEY` or `llm.api_key` in `~/.rijo/config.yaml`. Any OpenAI-compatible
endpoint works; `--provider` selects a built-in template, `--base-url` and `--config`
override for one run.

Install to `~/.local/bin/rijo` without `sudo`:

```bash
./install.sh
```

### Tests

```bash
cargo test -p rijo-core -p rijo-tui
cargo clippy --workspace --all-targets -- -D warnings
```

---

## How it works

```
rijo (CLI/TUI) ──► rijo-core: executor ──► LLM (streaming) ──► tool registry ──► guard
                                         └──► brain (SQLite + embeddings)
```

- **One turn loop** (`executor.rs`): context assembly → optional router → streaming LLM call
  → parallel read-only tool batch → guards → persistence.
- **The guard applies to every execution path** (`terminal`, `execute_code`, script contents,
  MCP, box crossings). It is a text heuristic, **not** a sandbox — documented as such.
- **The registry is the single source of truth** for tools. Names stay sorted so the prompt
  prefix stays cacheable.
- **A frontend never reimplements core knowledge**: it calls `inspect` and receives `UiEvent`.

Full map: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

---

## Documentation

| I need … | Read |
|---|---|
| The architecture and feature map | [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) |
| The open / closed boundary | [`docs/STRUCTURE.md`](docs/STRUCTURE.md) |
| What is proven and what is open | [`docs/status.md`](docs/status.md) |
| One feature in depth | [`docs/features/`](docs/features/index.md) |
| MCP server and client | [`docs/MCP.md`](docs/MCP.md) |
| Install and release | [`docs/INSTALL.md`](docs/INSTALL.md), [`docs/RELEASE.md`](docs/RELEASE.md) |
| History, measurements, old TUI docs | [`docs/history/`](docs/history/README.md) |

---

## License

Licensed under either of

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
- MIT license ([`LICENSE-MIT`](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this project shall be dual licensed as above, without additional
terms or conditions. See [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`DCO`](DCO).
