# Rijo Code Onboarding & TUI — Plan

Status: **implemented** — kept as the original design record. The wizard below
shipped as `crates/rijo-core/src/onboarding.rs`: `rijo` onboards on first start
(missing `~/.rijo/config.yaml`), and `RIJO_SKIP_ONBOARD=1` bypasses it (used by the
CI/release smoke tests). The present-tense steps are therefore **not** a to-do
list; they describe what was built.

Goal: first start of `rijo` → a running agent session in under 5 minutes.
The wizard handles the provider choice + key validation + E2E smoke test once;
after that Rijo Code starts directly into the TUI.

---

## 1. Onboarding (on first start)

**Trigger:** no `~/.rijo/config.yaml` OR `onboarded: false` in it.

**Wizard steps (TUI, dialoguer fallback on non-TTY):**

1. **Welcome** (2 lines)
   > Rijo Code — native-speed coding agent (Rust).
   > Setup: choose provider → enter key → smoke test.

2. **Choose provider**
   ```
   [1] opencode-go   – $10/month, fixed limits, coding-optimized   (recommended)
   [2] deepinfra     – pay-per-use, 100+ open models
   [3] custom        – any OpenAI-compatible base_url + key
   ```

3. **Key entry** (hidden) with **live validation**:
   - opencode: `GET https://opencode.ai/zen/go/v1/models`
   - deepinfra: `POST .../chat/completions` mini-request ("say ok", max_tokens 3)
   - Validation fails → plain-text error + retry (max 3×)
   - Offer the provider's models directly as a choice (deepinfra: `/models`
     listed plus top-10 coders; opencode: model catalog as an SPA list).

4. **Workspace** — default directory (Enter for `~`).

5. **Smoke test** — a real `terminal` tool call `echo ok` in the agent loop.
   If that is ok: onboarded = true, config written (chmod 600).

**Persistence:** everything in `~/.rijo/config.yaml` + `chmod 600` (the key is
never logged in plain text, only the last 4 characters in the summary).

**Onboarding repair:** `rijo --onboard` overwrites the provider setup.

---

## 2. Terminal UI (Ratatui + Tokio)

**Layout in 3 columns:**

```
┌─ session ─────────────────────┬─ stream ────────────────────────────────┐
│ ▶ current | 4 turns           │ [tool]  terminal "ls -la"   24ms  ✓     │
│ 2026-09-04 e2e_webapp       │ [assistant] Build a small webapp…       │
│ 2026-09-03 py-refactor      │ [tool]  read_file Cargo.toml 2ms ✓     │
│ …                            │ [assistant] Done — 3 files…            │
│                                                        │
├─ trace / tools ────────────────────────────────────────────────────────┤
│ tool       count  p50  p95 │ tokens: in 12.4k · out 8.2k · $0.014     │
└────────────────────────────────────────────────────────────────────────┘
```

- **Middle column**: conversational stream (user/assistant/tool events
  with color: blue=assistant, green=tool-ok, red=tool-fail, gray=info)
- **Sidebar**: session list (persisted), + trace stats + token budget
- **Input**: `/` for slash commands (`/exit`, `/tools`, `/memory`, `/session …`,
  `/bench`), Enter sends, Ctrl-C aborts the turn
- **Streaming**: SSE-capable (chat completions via `stream:true` +
  `event-stream` feature of ratatui + tokio::select! in the loop)

**Components:**
- `tui/state.rs` — collect from ChatHistory + tool_trace (shared via Arc)
- `tui/events.rs` — crossterm EventStream → Action enum
- `tui/render.rs` — the 3 column widgets (List, Paragraph, Gauge)
- `tui/mod.rs` — prompt → run_turn with tokio select! (abort on Ctrl-C)

**Dependencies:** `ratatui`, `crossterm` (with `event-stream`), `dialoguer`
(wizard fallback), `arboard` optional for clipboard copy.

---

## Implementation order

| # | Task | Effort |
|---|---|---|
| 1 | Onboarding wizard (provider choice, key validation, chmod 600, smoke test) | small |
| 2 | Ratatui chat view with stream + tool-trace panel | medium |
| 3 | Session list + persistence (`data/sessions.db`) | medium |
| 4 | Streaming (SSE) token by token in the stream field | small |
| 5 | E2E runner extended: per-run token cost | small |

Output goal: running `rijo` standalone starts → wizard or TUI (depending on state)
and both together respect the global tracing convention.
