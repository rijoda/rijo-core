# Repository structure — open engine, closed workbench

**Purpose:** One page that shows what belongs to the open engine, what belongs to the
closed desktop app, and which direction dependencies may point.

Decision:
[`decisions/0001-open-core-closed-workbench.md`](decisions/0001-open-core-closed-workbench.md).

---

## The open repository

```
rijo-core/
├── Cargo.toml                 # workspace: rijo-core, rijo-cli, rijo-tui
├── Cargo.lock                 # open dependency graph only — no Tauri
├── .cargo/config.toml         # ORT_DYLIB_PATH → vendor/onnxruntime/
├── crates/
│   ├── rijo-core/             # engine: executor, 34 tools, guard, sessions,
│   │                          # brain, router, llm, config, ui_events, inspect
│   ├── rijo-cli/              # binary `rijo`: headless + MCP
│   └── rijo-tui/              # frozen terminal frontend (crossterm/ratatui)
├── vendor/
│   └── onnxruntime/           # third-party runtime (MIT)
└── docs/                      # open documentation
```

The **Rijo Code desktop workbench** is a separate, closed repository. It is not in this
tree, and nothing in this tree depends on it.

---

## Dependency direction

```
          ┌──────────────────────────── open ────────────────────────────┐
          │                                                              │
          │   rijo-core ──► rijo-cli                                     │
          │       │  └────► rijo-tui                                     │
          │       │                                                      │
          └───────┼──────────────────────────────────────────────────────┘
                  │  public surface only
          ┌───────┼──────────────────── closed ──────────────────────────┐
          │       ▼                                                      │
          │   Rijo Code desktop (separate repository, not public)        │
          └──────────────────────────────────────────────────────────────┘
```

| Rule | Why |
|---|---|
| `rijo-core` depends on **no** UI crate (no tui, no crossterm, no Tauri) | the open engine must build without a frontend |
| a frontend may use `ui_events`, `inspect`, `executor`, `tools`, `config`, `llm` | one source of truth for tools, approvals and queries |
| a frontend never parses tool payloads on its own | the desktop would otherwise drift from the CLI |
| the desktop never reaches into `rijo_tui` | deleting the TUI must not break the app |

The desktop is **in-process orchestration**: its Tauri commands call the core's executor and
registry directly. There is no second HTTP daemon and no second tool implementation.
MCP (`rijo mcp serve`) exposes the same registry for headless clients.

---

## Commands

```bash
cargo build --release -p rijo
cargo test -p rijo-core -p rijo-tui
cargo clippy --workspace --all-targets -- -D warnings
cargo package -p rijo-core
```

---

## Proof that the boundary holds

- `cargo metadata --no-deps` lists only `rijo`, `rijo-core` and `rijo-tui`.
- `Cargo.lock` contains no `tauri` entry.
- `cargo tree -p rijo-core` contains no `ratatui`, `crossterm`, `nucleo`,
  `tui-panel-select` or `tauri`.
- `UiEvent` in `rijo-core::ui_events` has no terminal or GUI type; the TUI wraps it in
  `rijo_tui::TuiEvent`.
