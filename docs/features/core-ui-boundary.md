## Core / UI boundary

**Status:** ✅ **Complete (2026-11-08)** — TUI extracted, UI dependencies removed, the open
and closed workspaces are separated; TUI frozen since 2026-10-18.
**Why:** the core should be open-source on its own; the desktop GUI is not.
That only works if the core never depends on a UI and the GUI never reimplements
core knowledge. Decision:
[`../decisions/0001-open-core-closed-workbench.md`](../decisions/0001-open-core-closed-workbench.md).

### What the completed split looks like (2026-11-08)

| Before | After |
|---|---|
| `rijo-core::tui` (9,130 lines) + `rijo-core::clipboard` | `crates/rijo-tui`, one frontend crate |
| `crate::tui::TuiEvent` re-exported `UiEvent` | `TuiEvent` wraps `rijo_core::ui_events::UiEvent` + terminal input |
| `UiEvent::Key/Mouse/Paste/Clipboard` (crossterm types in the core) | terminal-only variants live in `rijo_tui::TuiEvent` |
| `ratatui`, `crossterm`, `nucleo`, `tui-panel-select`, `syntect`, `textwrap` in `rijo-core` | all in `rijo-tui` |
| `rijo-desktop` a member of the root workspace | a separate, closed repository |

Mechanical proof: `cargo check -p rijo-core` succeeds in this repository; the CI job
**core only (open workspace)** asserts that no desktop crate is in `cargo metadata`.

### What moved (2026-10-17)

| Was | Now |
|---|---|
| `TuiEvent`, `ConfirmTx`, `AskQuestion`, `AskOption` | `rijo_core::ui_events` (renamed `UiEvent`) |
| `ChatEntry`, `EntryRole` (transcript model) | `rijo_core::ui_events` |
| `HistoryHit`, `SearchMode` | `rijo_core::ui_events` |
| `AppState::tui_tx` | `AppState::ui_tx`, typed `UnboundedSender<UiEvent>` |
| Tool catalog, runtime facts, panel data, workspace status | `rijo_core::inspect` |

`rijo-tui` imports `ui_events` and wraps it in its own `TuiEvent`. The closed desktop app
imports `ui_events` and `inspect` directly; it never depends on `rijo_tui`. Deleting the
TUI would not break the app.

### The rule

- **Core** owns execution, tools, guards, sessions, the transcript model, the
  event contract and every read-only query (`inspect`).
- **A frontend** owns rendering, input, layout and its own process/window model.
- A frontend may call `inspect` and receive `UiEvent`; it may not reach into
  `tui`, and it may not parse tool payloads on its own.

### Follow-ups inside `rijo-tui` (no longer blocking publication)

The extraction moved these helpers with the crate, so they no longer pull UI code into the
core. Sharing them with the GUI is still desirable:

| Item | Should become |
|---|---|
| `smart_excerpt` | `summarize::tool_result(tool, args, result)` — TUI prints a human line, GUI prints the raw excerpt |
| `ToolCatalogEntry` + builder | already in `inspect::tool_catalog`; TUI still builds its own |
| `OperationsItem` + `load_cron_summaries` | already in `inspect::panel("operations")` |
| `replace_conversation_with_session` | `core::sessions::apply_history` |
| `start_history_search` + panel state | `core::sessions::search` (the tool exists; the shaping is duplicated) |
| `build_runtime_lines` | presentation over `inspect::runtime_info` |
| `compact_transient_entries` | transcript policy shared with the GUI |

### TUI-only, stays with the terminal frontend

- Rendering (`render_*`, `draw`, ~1,700 lines) and the theme.
- Input handling, paste heuristics, scroll, viewport, status line.
- `run_inner` (the terminal event loop) and `execute_command` (slash commands).
- The `display_tests` snapshot suite (2,694 lines).

### Layout now in place

```
Cargo.toml                 open workspace
crates/rijo-core/          public: executor, tools, guard, sessions, ui_events,
                           inspect, summarize, brain, llm, providers
crates/rijo-cli/           public: binary `rijo`, headless + MCP
crates/rijo-tui/           public: frozen terminal frontend
```

The closed desktop app lives in a separate repository (`rijoda/rijo-code`). It may only
use `ui_events`, `inspect`, `executor`, `tools`, `config` and `llm` from the core.
