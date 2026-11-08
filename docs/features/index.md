# Features — Index

This directory documents individual Rijo Core features in detail. It is
**partial** on purpose: the current state lives in
[`../status.md`](../status.md) and [`../ARCHITECTURE.md`](../ARCHITECTURE.md); the files
below go deeper on the features that have a dedicated write-up.

## Core / UI

- [Repository structure](../STRUCTURE.md) — the open engine and the closed workbench
- [Core / UI boundary](core-ui-boundary.md) — UI contract, inspection API, the extraction

## Security

- [File Safety](security-file-safety.md) — delete restrictions, workspace scope
- [Env Key Isolation](security-env.md) — `.env` protection, sealed arena
- [Trust System](security-trust.md) — workspace trust prompt, marker files

## Browser

- [Browser devtools](browser-devtools.md) — `browser_dev`: screenshot, evaluate, console,
  network, tabs, cookies, PDF, viewport (CDP)

## Elsewhere

The tools, the brain (SQLite index + embeddings + FTS5), the tool router, MCP
(both directions), the context tool and the sync core are documented in
[`../ARCHITECTURE.md`](../ARCHITECTURE.md) and [`../MCP.md`](../MCP.md) rather than in
separate feature files here.

The legacy TUI documentation moved to [`../history/tui/`](../history/tui/README.md)
when the interface was frozen on 2026-10-18.
