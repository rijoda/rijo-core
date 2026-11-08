# Desktop workbench (closed)

**Status:** separate repository, not part of this project.

The **Rijo Code** desktop workbench (Tauri 2 + HTML UI) is a closed application. Its source,
build, product record and detailed design documentation live in the private
`rijoda/rijo-code` repository.

This public repository documents only the contract the app consumes:

- `rijo_core::ui_events` — transcript, progress, approvals and questions
- `rijo_core::inspect` — tool catalog, runtime facts, panel data and workspace status
- `rijo_core::executor`, `tools`, `config`, `llm` — the same engine the CLI uses

The app never reaches into `rijo_tui`, never opens a second tool implementation, and never
bypasses `guard`. The rule and the rationale:
[`core-ui-boundary.md`](core-ui-boundary.md) and
[`../decisions/0001-open-core-closed-workbench.md`](../decisions/0001-open-core-closed-workbench.md).
