# Architecture & feature map

What Rijo Code is, which features exist today, and how they connect. The
*measured* state behind each claim lives in [`status.md`](status.md); this
file is the map. **34 native tools** (`rijo --tools`), plus a config-gated `box`
toolset.

Three crates in this open workspace:

- **`rijo-core`** — Rijo Core: executor, tools, guard, sessions, brain, router, and the
  shared UI contract (`ui_events`, `inspect`). No terminal or GUI dependency.
- **`rijo-cli`** — the binary `rijo` (thin, hot-patchable; `main.rs`, `mcp.rs`).
- **`rijo-tui`** — the frozen terminal frontend. It owns `crossterm`, `ratatui`,
  `nucleo`, `tui-panel-select`, clipboard access and its own input events; the core only
  publishes `UiEvent`s to it.

The **Rijo Code desktop workbench** is a separate, closed application. It is not in this
repository; it consumes the core's public surface (`ui_events`, `inspect`, `executor`,
`tools`, `config`, `llm`) and nothing else. The boundary rule:
[`features/core-ui-boundary.md`](features/core-ui-boundary.md). Decision:
[`decisions/0001-open-core-closed-workbench.md`](decisions/0001-open-core-closed-workbench.md).

---

## The spine: the turn loop (`executor.rs`)

One user message becomes a sequence of LLM iterations, each of which may call
tools. Around it sit the guards that keep a turn finite and honest.

1. **Context assembly** — system prompt (workspace + tech-stack + **skill index**
   + **brain pointers**) → tool schemas → history.
2. **Router** (`router/`) optionally trims the tool schemas to the relevant few.
3. **LLM call** (`llm.rs`) — OpenAI-compatible, **streaming** (text +
   `reasoning_content`), usage/token counts from the final SSE chunk.
4. **Tool calls** — read-only calls run as a **parallel batch**
   (`is_parallel_safe_call`); anything mutating/interactive breaks the batch and
   stays ordered.
5. **Guards** — repetition (`repetition_decision`), empty-response, and the
   **progress nudge** (10 non-mutating calls in a row → "act now").
6. **Result handling** — large outputs are capped and spilled to files
   (`tool_result_cap`), old ones stubbed (`tool_stub_after_messages`), history
   bounded (`max_messages`).
7. **Persistence** — the turn is written to `sessions.db`; metrics to
   `turn_metrics.jsonl`; the router decision to `decision_trace.jsonl`.
8. **Learning review** (`learning.rs`) — background: may create/update a skill or
   memory from the turn.

---

## Features, by area

### Models & providers (`llm.rs`, `providers.rs`, `models.rs`, `onboarding.rs`, `local_server.rs`)
- OpenAI-compatible HTTP; streaming tool calls; multimodal (image `image_url`).
- Provider **templates**: DeepSeek, OpenCode Zen/Go; generic `--base-url`.
- Token from the provider's own env var or its credential file (never copied).
- Model → context-window table; `--local <gguf>` owns a persistent `llama-server`.
- `RIJO_LLM_MOCK` replays scripted turns for offline tests.

### Tools (33, `tools/`)
| Toolset | Tools |
|---|---|
| file | `read_file` `write_file` `patch` `search_files` `delete_file` |
| terminal | `terminal` `process_manage` |
| git | `git` |
| web | `web_search` (flat provider list) `web_extract` |
| browser | `browser_navigate` `browser_snapshot` `browser_click` `browser_type` `browser_scroll` `browser_dev` (CDP) |
| research | `arxiv` |
| brain | `brain_search` `brain_outline` |
| skills | `skills_list` `skill_view` `skill_manage` |
| github | `github` (native REST, cached reads) |
| integration | `mcp` |
| core | `clarify` `context` `cronjob_manage` `delegate_task` `execute_code` `memory` `session_search` `text_to_speech` `todo_list` `vision_analyze` |

The **registry** (`tools/mod.rs`) is the single source of truth: names are
**sorted** (the prompt-cache invariant), each tool declares a static `ToolMeta` +
JSON schema. `box` (see below) is registered only when a box is configured.

Browser automation stays on **Chromium via CDP** (decision
[`0002`](decisions/0002-browser-cdp-chromium.md)); the box's `box_browser_*` tools are a
separate Servo path and are not merged into `browser_*`.

### Brain (`brain/`)
SQLite (`nodes`, `edges` [unused], `indexed_files`) + fastembed embeddings +
FTS5. Chunked per language, embedded, retrieved **hybrid** (vector + FTS5, fused
by reciprocal rank). Incremental (mtime/size/chunk-count), workspace-scoped.
Per turn the top hits are injected as **pointers** into the system prompt;
`brain_search` is the on-demand tool.

### Router (`router/`) — tool-schema selection
XLM-R base ONNX (4 heads: tools/toolset/guard/compact). `route_tools` sends only
the tools above threshold, **fail-open** (any error → full list). Off by default;
here enabled. Measured: −94 % schema chars, tool choice 79 %→90 % (99 runs),
−50 % prompt tokens over a 6-turn session. (`history/goal-100-de.md` §Y.2)

### Safety: Guard + Sandbox (`guard.rs`, `sandbox.rs`)
- **Guard** — one gate over **all** execution paths (`terminal`, `execute_code`,
  script contents): destructive patterns → user dialog (TUI), denied by default
  headless, every decision to `guard_trace.jsonl`. A text heuristic, *not* a sandbox.
- **Sandbox** — a seam for `terminal`/`execute_code`: `off` (default) / `bwrap`
  (Linux) / `box` (Rijo Box, **fail-closed** while the client is pending). Never
  silently runs on the host. (ADR 0018)

### Context management (`context_tool.rs`, `compress.rs`)
- Structural pruning in the loop (cap / stub / max_messages).
- **Billed volume vs context fill.** `TurnStats.prompt_tokens` is the sum of the
  provider-reported prompt tokens over every iteration of a turn — that is what the
  provider bills, and with 20+ tool iterations it can reach hundreds of thousands.
  `TurnStats.last_prompt_tokens` is the prompt size of the **last** request and the only
  number that describes the context fill. The TUI gauge and the desktop meter use the
  latter; the cumulative number stays visible as "billed". Measured 2026-10-18: a
  22-iteration website audit billed 335k prompt tokens while its last request was ~6k.
- **Defaults (2026-10-18):** `tool_result_cap` 6000 chars, `tool_stub_after_messages` 6,
  `router.max_tools` 6 predicted tools (always-keep added after). A/B and caveats:
  [`research/2026-10-18-context-budget-ab.md`](research/2026-10-18-context-budget-ab.md).
- `context` tool: `status` (fill vs. window), `recall` (FTS5 over this session),
  `show` (stored blocks).
- Compression **core** (`compress.rs`): plan / prompt / parse / `summarize`
  (LLM), watermark-incremental. The executor **trigger** is the next step (P4).

### Learning (`learning.rs`, `tools/skills_tool.rs`, `tools/memory_tool.rs`)
After a substantive turn a cheap model reviews it and may emit `SKILL_CREATE` /
`SKILL_UPDATE` / `MEMORY`. Writes go only to `~/.rijo/skills` and
`~/.rijo/data/memories`; foreign skill libraries are read-only. The skill index
sits at the **end** of the system prompt (cache-friendly).

### Sessions & history (`tools/session_tool.rs`)
`sessions.db`: `messages` + FTS5 + `summaries` (watermark). `session_search`
searches past sessions; `--resume` reloads the last 20 messages; `context recall`
searches the current one.

### MCP — both directions (`rijo-cli/src/mcp.rs`, `mcp_client.rs`, `tools/mcp_tool.rs`)
- **Server**: `rijo mcp serve` exposes the 34 tools over stdio as two meta-tools
  (`list_tools`/`call_tool`), Guard-routed. Config example in `docs/MCP.md`.
- **Client**: the `mcp` tool consumes **external** servers (stdio); a remote tool
  runs without approval only if it is in the server's `read_only_tools`, else the
  Guard gates it. (ADR 0017)

### Rijo Box (`box_client.rs`, `tools/box_tools.rs`) — ADR 0010/0011
Config-gated (`RIJO_BOX_ADDR`): registers the `box` toolset (exec/files/agent in
the guest) only when a box is configured, so it costs no prompt weight otherwise.
The VM is the containment boundary; `terminal`/`execute_code` route into the
guest when a box is active (the sandbox `box` backend).

### Desktop workbench — separate repository, closed
The Rijo Code desktop workbench (Tauri 2 + HTML UI) is not part of this repository. It
consumes `UiEvent`s and `inspect`, and answers approvals and `clarify` questions through
the same one-shot channels the TUI uses. Its source, build and release live in the private
`rijoda/rijo-code` repository; only its public contract is documented here
([`features/core-ui-boundary.md`](features/core-ui-boundary.md)).

### TUI (`crates/rijo-tui`) — legacy, frozen
Two permanent rows (activity + measurements) with a context/cache **gauge** and
**clickable** rows; a **todo strip** (Ctrl+T); an interactive **clarify** overlay;
attachments + clipboard image paste (`Ctrl+V`/`/paste`); command palette,
scrolling, minimap (unwired), `--tui-snapshot`.

**Frozen since 2026-10-18.** Bug fixes only; the TUI is rebuilt after the desktop covers
the remaining workflows. Old documentation: [`history/tui/`](history/tui/README.md).

### Cross-cutting
- **Summaries** (`summarize.rs`): one human target per tool call (`terminal · cargo`,
  `read_file · lib.rs`), shared by every frontend. Result summaries are still TUI-only
  (`smart_excerpt`).
- **Trust & secrets** (`trust.rs`): workspace-trust prompt; sealed `.env` (keys
  never exposed to the model); `.env` files refused by file tools.
- **Processes** (`process.rs`): background registry, process groups, `kill_tree`.
- **Sync** (`sync.rs`): portable export/import (brain nodes, skills, memory) —
  Git transport is the next step.
- **Self-update / hot-reload** (`selfupd.rs`, `watcher.rs`): staged binary swap
  and restart; config/skill hot-reload via the file watcher.
- **Platform** (`paths.rs`): one home resolution (`~/.rijo`), cross-platform
  `which`, Windows/Linux parity.
- **Observability** (`metrics.rs`, `debug.rs`, `bench.rs`, `schema_eval.rs`):
  `turn_metrics.jsonl`, `tool_trace.jsonl`, `decision_trace.jsonl`,
  `guard_trace.jsonl`; `rijo --bench`, `--schema-eval`.

---

## How it fits together (one turn)

```
user message
   │
   ▼
Executor ── system prompt = workspace + tech-stack + skill index + brain pointers
   │                                   ▲                     ▲
   │                                   │ learning.rs         │ brain/
   ▼                                   │ (skills)            │ (indexed pointers)
Router (XLM-R) ── relevant tool schemas only (fail-open)
   ▼
LLM (streaming) ──────────────► tool calls
   │                                │
   │                                ▼
   │                     Guard (destructive? → dialog / deny)
   │                                │
   │                     Sandbox backend: off | bwrap | box
   │                                │
   │                     tool executes (parallel if read-only)
   │                                ▼
   └──────── result ◄── prune/cap/stub, persist to sessions.db + metrics
   │
   ▼
final answer ── learning review (background) ──► skill/memory ──► next turn's prompt
```

**Invariants that hold the design together:**
1. **Sorted tool names** → byte-stable prompt prefix → provider cache (94 %).
2. **The Guard applies to every execution path** (incl. MCP and box crossings).
3. **Fail-open, never silent:** router, brain and box fall back to "full/off" and
   *say so* (trace); the sandbox refuses rather than downgrade.
4. **Honest limits named in code and docs** (heuristic ≠ sandbox; recall is
   lexical-first; first-turn token win, cached later).
