# 0001 — Rijo Core and Rijo Code: open engine, closed workbench

**Status:** accepted

**Date:** 2026-10-18

**Purpose:** Fix what "Rijo Code" means after the product changed from a TUI CLI to a
desktop app on top of a reusable engine, and fix which part is open source.

> Project-level decision. A headquarters ADR can restate it when the repository split
> actually happens; this file is the binding record for the code in this repository.

---

## Context

- The agent has shipped since 2026-09-30 as **Rijo Code CLI**. That plan assumed a CLI now
  and a "Rijo Code GUI" later.
- Since 2026-10-17 there is a Tauri desktop workbench (`app/crates/rijo-desktop` since
  2026-11-08), and it is
  meant to become the interactive product surface. The CLI and MCP stay as headless entry
  points; the TUI is to be rebuilt from scratch later and receives no new features.
- The intended split is written down in
  [`../features/core-ui-boundary.md`](../features/core-ui-boundary.md): the engine should be
  open source, the desktop GUI should not. Today none of that is enforced:
  - one repository, one Cargo workspace, one lockfile;
  - `rijo-desktop` is a workspace member, so every workspace build and every CI run includes
    the closed frontend;
  - `rijo-core` still contains `tui.rs` (9,840 lines, 22 % of the crate) and depends on
    `ratatui`, `crossterm`, `tui-panel-select` and `nucleo`.

So both statements — "the core is open source" and "the frontend is not" — are currently
untrue in the repository layout.

## Options

| Option | Advantage | Disadvantage |
|---|---|---|
| A — one monorepo, no split | no migration effort; one version | the repo cannot be public while the desktop lives in it; "open core" stays a claim; every core user downloads the closed UI |
| B — **two repos**: `rijo-core` public, `rijo-code` private | the open/closed line is real and testable; one version pin; the desktop can iterate fast | two CI setups; the core needs its own release cadence; the TUI must leave the core crate |
| C — three repos: core, CLI, desktop | cleanest separation | three release trains and three version pins for a one-person lab; no current benefit |

## Decision

**Chosen: Option B.**

| Layer | Name | Repo | Visibility | Contains |
|---|---|---|---|---|
| Engine | **Rijo Core** | `rijoda/rijo-core` | public | executor, tools, guard, sessions, transcript model, UI event contract, `inspect`, LLM, brain, config |
| CLI | `rijo` (Rijo Code CLI) | `rijoda/rijo-core` | public | headless runs, MCP server/client; legacy TUI until it is rebuilt |
| App | **Rijo Code** | `rijoda/rijo-code` | private | `rijo-desktop`: Tauri shell, HTML UI, window/project model |

The desktop keeps the product name **Rijo Code**. The earlier suffix plan ("Rijo Code GUI")
is dropped. "Rijo Code" in prose means the product/app; **Rijo Core** means the engine.

### The boundary rule

- **Core owns** execution, tools, guards, sessions, the transcript model, the event contract
  (`ui_events`) and every read-only query (`inspect`).
- **A frontend owns** rendering, input, layout and its own process/window model.
- A frontend may call `inspect` and receive `UiEvent`; it must not reach into `tui`, and it
  must not parse tool payloads on its own.
- **`rijo-core` must not depend on a terminal UI.** Concretely: no `ratatui`, `crossterm`,
  `tui-panel-select` or `nucleo` in its dependency list, and `cargo build -p rijo-core`
  must succeed in a checkout without the desktop crate.
- **TUI freeze:** no new TUI features. Bug fixes only, until the rebuild replaces it.

## Consequences

**Easier**

- "Open engine, closed workbench" becomes verifiable — a visible repo, a licence, and a build
  that does not contain the frontend.
- Contributors can build and test the core without Tauri, WebView2 or a UI toolchain.
- The desktop can move fast without publishing its internals.

**Harder / to note**

- Two repositories mean two release points and a version pin from the desktop to the core.
- The TUI extraction is a large mechanical change (≈9.8k lines plus 4 dependencies).
- Until the split is done, the rule is enforced by review, not by the build.

**Still to do**

1. ~~Close the open working tree and tag a checkpoint~~ — done 2026-10-18: commits
   `d18d121`…`b2ba060`, tag `pre-cleanup-2026-10-18`; the documentation cleanup follows in
   `b8aa3fc` and `25a8b49`.
2. ~~Move `tui.rs` and the UI-only helpers from `rijo-core` to a `rijo-tui` crate, remove
   the UI dependencies from the core.~~ **Done 2026-11-08** (`crates/rijo-tui`).
3. ~~Add a CI job that builds `rijo-core` alone.~~ **Done 2026-11-08** (*core only (open
   workspace)*), and `app/` is now a separate Cargo workspace. **Open:** make it a separate
   Git repository.
4. Publish `rijo-core` under `Apache-2.0`; keep the desktop repository private.

## Sources / Evidence

- [`../features/core-ui-boundary.md`](../features/core-ui-boundary.md) — the extraction that
  is already done (`ui_events`, `inspect`) and what is still core-shaped in `tui.rs`.
- The closed workbench's own documentation (product record, desktop design, implementation)
  lives in the private `rijoda/rijo-code` repository.
- Measured on 2026-10-18: `rijo-core` 43,856 lines, of which `tui.rs` 9,840; `rijo-cli` 963;
  `rijo-desktop` 1,440 Rust + 1,732 JS lines.
