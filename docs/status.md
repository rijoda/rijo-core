# Status

**Purpose:** The short, current picture — what is proven, what is being built, what is open.
The detailed measurement record lives in [`history/goal-100-de.md`](history/goal-100-de.md)
and the chronological notes in [`history/readme-2026-10-18.md`](history/readme-2026-10-18.md).

**Updated:** 2026-11-08

---

## Proven

| Claim | Value | Where the evidence is |
|---|---|---|
| Tool latency vs. the Python reference | **188×–1037×** faster | history README, benchmark section |
| Parallel read-only tool batches | **3.83×** local, **3.69×** network | history README |
| Prompt-cache hit rate | **26 % → 94 %** (sorted tool schemas) | history README |
| Brain warm start | **65 s → 18 ms** (incremental indexing) | history README |
| Terminal latency | 50 ms poll sleep removed, **~20×** | history README |
| Router (tool-schema selection) | **−94 %** schema characters, tool choice 79 % → 90 % (99 runs) | `history/goal-100-de.md` §Y.2 |
| Safety gate | tested on all execution paths, headless fails closed | `features/security-*.md` |

All numbers were measured on real runs; the scripts and fixtures are in `data/` and
`scripts/`. Nothing in this table is an estimate.

## Built

- **34 native tools** (`rijo --tools`), registry as the single source of truth.
- **Brain:** SQLite + fastembed, hybrid retrieval (vector + FTS5, RRF), incremental.
- **Guard:** one gate over `terminal`, `execute_code`, script contents, MCP and box
  crossings, with an audit trail.
- **MCP both directions:** `rijo mcp serve` (two meta-tools over the registry) and an
  MCP client tool.
- **UI contract:** `ui_events` + `inspect` are in the core. The desktop workbench is a
  separate, closed application; it consumes only this contract and never the TUI.

## In progress

1. **Desktop parity** — the closed Rijo Code workbench replaces the TUI once the remaining
   workflows are covered. This is tracked in the private app repository.
2. **First public release** — publish `rijo-core` under `Apache-2.0`, then a
   `0.x` release of the `rijo` binary (decision:
   [`decisions/0001-open-core-closed-workbench.md`](decisions/0001-open-core-closed-workbench.md)).

## Done (2026-11-08)

- **Core / UI split** — `tui.rs` (9,130 lines) and `clipboard.rs` moved to
  `crates/rijo-tui`; `ratatui`, `crossterm`, `nucleo`, `tui-panel-select`, `syntect` and
  `textwrap` are gone from `rijo-core`. `UiEvent` is now terminal-free; the TUI wraps it in
  its own `TuiEvent`.
- **Repository split** — this repository contains only the open engine, CLI and TUI. The
  closed desktop workbench lives in `rijoda/rijo-code`.
- **Proof** — `cargo test -p rijo-core -p rijo-tui`: 485 passed / 7 ignored on
  Windows, 495 passed / 7 ignored on Linux (two `cfg(unix)`-only tests);
  `cargo clippy --workspace --all-targets -- -D warnings` clean;
  `cargo tree -p rijo-core` has no UI dependency.
- **CI** — the job **core only (open workspace)** runs `cargo check -p rijo-core` and
  asserts that no desktop crate is part of the workspace.

## Known limits (named, not hidden)

- **The guard is a text heuristic, not a sandbox.** An interpreter switch, a dropped script,
  base64 or runtime assembly gets past it. The real boundary is the Rijo Box.
- **The brain has no graph layer:** the `edges` table exists but nothing writes to it.
- **CI is green here.** The first public matrix passed on Linux and Windows (run
  `42502279726`, 2026-11-08), including the *core only (open workspace)* job. The private
  development repository had a GitHub Actions billing block since 2026-10-13; local runs
  remain the source of truth for the performance numbers.
- **The TUI is frozen** and will be rebuilt, not extended.

## Next after the cleanup

1. ~~Move the TUI out of `rijo-core` and drop the UI dependencies.~~ **Done 2026-11-08.**
2. Publish the core under `Apache-2.0` and ship a `0.x` CLI release.
3. Finish the desktop workflows (private app), then retire the TUI.
