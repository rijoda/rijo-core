# Documentation

**Purpose:** Map of the active documentation. Start at the repository
[`README.md`](../README.md).

| File | What it answers |
|---|---|
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | How the crates and the turn loop fit together; feature map |
| [`status.md`](status.md) | What is proven, what is built, what is open, known limits |
| [`MCP.md`](MCP.md) | The MCP server and client: surface, config, guard behaviour |
| [`INSTALL.md`](INSTALL.md) | Install, build, platform notes |
| [`RELEASE.md`](RELEASE.md) | Release checklist and artefacts |
| [`web-search-plan.md`](web-search-plan.md) | Plan and measurements for the web layer |
| [`research/2026-10-18-browser-engine.md`](research/2026-10-18-browser-engine.md) | Browser engine: Servo vs Chromium, measured state, recommendation |
| [`research/2026-10-18-context-budget-ab.md`](research/2026-10-18-context-budget-ab.md) | A/B of the tighter context defaults, with the honest caveats |
| [`decisions/`](decisions/0001-open-core-closed-workbench.md) | Project-level decisions (open core, closed workbench) |
| [`decisions/0002-browser-cdp-chromium.md`](decisions/0002-browser-cdp-chromium.md) | Browser: stay on Chromium/CDP |

## Features

[`features/index.md`](features/index.md) — one file per implemented feature:
core/UI boundary, security (env, files, trust), browser devtools.

## History

[`history/`](history/README.md) — the chronological record, the German goal document, the
early quickstart, and the old TUI documentation. Kept as evidence, not as current guidance.

## Rules

- Active documentation is **English** (ADR 0009 in the headquarters).
- Every claim carries a measurement or is marked as open.
- One topic per file; link instead of copying.
