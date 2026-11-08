## TUI: Entry Rendering

**File:** `src/tui.rs` (functions `render_chat`, `role_label_span`, `entry_header_span`)  
**Status:** ✅ Implemented  
**Since:** Phase 3 (2026-09-06, redesigned)

### Layout

Every chat entry (user, assistant, tool, error, status) is separated by a
decent `─` line in DarkGray. Individual entries get a header line once:

```
──  rijo                   ← Assistant header (yellow, once per entry)
Answer text here…
further text…

──  you                    ← User header (cyan)
prompt text

─                               ← separator
── ✗ error                    ← Error header (red)
error message

◌ web_search query…          ← Tool call start (no header)
✓ web_search · 320ms  │ 3 results  ← Tool done
```

### Label rules

| Role | Header | Per-line label |
|------|--------|----------------|
| User | `──  you ` (cyan) | `  ` (2 spaces, bold cyan) |
| Assistant | `──  rijo ` (yellow) | empty |
| Tool | none | empty |
| Error | `── ✗ error ` (red) | empty |
| Status | none | empty |

### Separator

A `─` line in `Color::Rgb(40, 45, 55)` is drawn between entries. No separator
before the very first entry.

## Commands and capability discovery

- `/tools` opens a searchable directory built from the live tool registry. It shows
  each registered tool's toolset, description, required arguments, and action enum
  when present. Optional tools appear only when registered.
- The directory is metadata-only: selecting a tool inserts a prompt into the
  composer; it does not dispatch the tool or bypass the normal registry/approval
  path. `Ctrl+P` opens the shared command palette.
- `/runtime` shows the configured model, enabled subsystems, sandbox, router,
  MCP configuration, memory-store byte counts and CLI-only maintenance commands.
  Credentials are represented only as configured/missing; memory contents and MCP
  connectivity are not read/probed by opening the panel.
- `/operations` lists current background-process IDs/status, todos, and scheduled-job
  IDs. Selecting an item inserts a prompt; it does not mutate or control the record
  directly. Process actions still go through `process_manage` and its existing
  contracts. Scheduled-job prompt contents are not displayed.
- Very old transient status/tool/error/thinking rows are compacted once the transcript
  grows large. User/assistant messages and active tool rows are retained; the marker
  reports the compaction. `/activity` searches the most recent 512 compact summaries
  (raw tool details for compacted rows are not archived).
- `/history` (`/knowledge` alias) searches saved sessions through the registered
  `session_search` tool. Results retain session/role/time provenance; selecting one
  with Enter inserts a bounded excerpt as context. Ctrl+R asks for confirmation,
  loads that session's recent messages and continues writing to that session; the
  current in-memory conversation is replaced only after approval.
- `/brain` searches the indexed workspace through `brain_search`. It shows source,
  kind, score and a bounded excerpt; selecting a result inserts context but does not
  open or modify the source file.
- `/skills` browses skills discovered from the configured safe roots. Selecting a
  skill inserts a prompt to load it through the registered skill tools.
- `/context` prints the known context-window size and last provider token count; use
  the `context` tool for stored compression blocks or recall details.
- `/memory` displays the `MEMORY.md` and `USER.md` entries in a read-only panel.
  Selecting an entry inserts it into the composer as context; the saved stores are
  never modified by browsing.
- `/thinking min|med|max` applies to the next TUI turn only. It does not mutate the
  saved provider configuration; unsupported providers omit the reasoning-effort
  request field as before.
- `/clear` clears the visible transcript and in-memory conversation context. It does
  not delete persisted session records.

## Input keys

| Key | Behavior |
|---|---|
| Enter | Send prompt |
| Alt/Shift+Enter | Insert newline |
| Ctrl+C | Quit while idle; request cooperative cancellation while a turn runs |
| Esc | Close the active overlay; otherwise request cancellation while running |
| Ctrl+K | Expand/collapse thinking entries |
| Ctrl+O | Toggle full tool/reasoning details |
| Ctrl+V | Paste text or attach a clipboard image |
| Ctrl+R (History panel) | Resume the selected saved session after confirmation |
| Up/Down | Scroll when the composer is empty; otherwise recall input history |

Printable `q`, `c`, and `k` are ordinary composer input. Use `/exit` to quit and
`/copy` to copy the last assistant answer.

Idle timer checks may refresh todo metadata, but unchanged idle frames are not
repainted. Running turns retain the spinner/status refresh; edge scrolling uses a
faster tick while it is armed.
