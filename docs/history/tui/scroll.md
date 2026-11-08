## TUI: Scroll System

**File:** `src/tui.rs` (functions `scroll_up`, `scroll_down`, `scroll_pct`, `render_chat`)  
**Status:** ✅ Implemented  
**Since:** Phase 2–3

### Concepts

- **Follow mode** (`bottom_anchor = None`) — viewport auto-follows the bottom
  of the chat. New entries are visible immediately. Reset by pressing Enter
  to send a message, or `Alt+End`.

- **Scroll mode** (`bottom_anchor = Some(n)`) — viewport is anchored `n` lines
  above the bottom. New entries are pushed but the viewport stays. Indicated
  in the status line: `↑ 42% · End/PosDown=follow`.

### Caching

Every `draw()` cycle publishes three atomics:
- `CACHED_TOTAL` — total rendered lines
- `CACHED_VISIBLE` — visible lines in the chat area
- `CACHED_MAX_SCROLL` = `CACHED_TOTAL - CACHED_VISIBLE` (computed once)

All scroll handlers read from these caches, guaranteeing consistency.

Completed chat entries keep their wrapped/styled line layout in an entry cache keyed
by content, role, width and verbosity. Each frame copies only the visible line window
into Ratatui; the full flattened selection buffer is refreshed only when its source
signature changes. This keeps scroll offsets absolute while avoiding repeated
syntax-highlighting and Markdown-lite work for unchanged history.

### Clamping

`bottom_anchor` is clamped to `CACHED_MAX_SCROLL` on every `draw()` — so if
entries are removed or the terminal resized, the scroll position never exceeds
the available content.

### Keys

| Key | Action |
|-----|--------|
| PgUp | Scroll up 10 lines |
| PgDown | Scroll down 10 lines |
| ↑ / ↓ | Scroll the chat **when the input is empty**; with text typed (or a history recall in progress) they walk the input history |
| Ctrl+↑ / Ctrl+↓ | Scroll one line |
| Mouse wheel (scroll up) | Scroll up 3 lines |
| Mouse wheel (scroll down) | Scroll down 3 lines |
| Alt+Home | Jump to top |
| Alt+End | Return to follow mode |
| Tab / Shift+Tab | Jump to next/previous user prompt |

> Some Windows terminals report the mouse wheel as ↑/↓ key events. Because the
> arrows scroll the chat while the input is empty, the wheel still scrolls the
> chat there. `Ctrl+↑/↓` had been shadowed by the unguarded arrow arm and never
> ran; the modifier arms now come first.

### Prompt jumping

`jump_prompt()` calculates the approximate line offset of each user entry
(via `prompt_line_offsets`) and scrolls so the prompt appears near the top.
Selected prompt is highlighted in the minimap.
