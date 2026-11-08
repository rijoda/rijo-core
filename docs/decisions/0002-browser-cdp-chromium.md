# 0002 — Browser automation stays on Chromium via CDP

**Status:** accepted

**Date:** 2026-10-18

**Purpose:** Record that Rijo Code does not switch its `browser_*` tools to a Rust-native
engine; Chromium over the Chrome DevTools Protocol remains the browser backend.

---

## Context

- The `browser_*` tools in `crates/rijo-core/src/tools/browser_tool.rs` launch or attach to
  a headless Chromium/Chrome and speak raw CDP over WebSocket: navigate, text snapshot,
  click, type, scroll. `browser_backend: cdp` gates the path.
- The browser layer was evaluated against Rust-native alternatives in
  [`../research/2026-10-18-browser-engine.md`](../research/2026-10-18-browser-engine.md).
  Result: **Servo** is the only serious Rust engine (crates.io `servo` 0.7.0, LTS), but its
  WebDriver surface has no console, network interception, performance tracing, PDF or the
  tab fidelity of CDP, and in-process embedding on Windows is blocked by the build
  toolchain (GNU fails in `glslopt`; MSVC needs VS Build Tools + SpiderMonkey).
- Blitz has no JavaScript engine (a renderer, not a browser). Verso is archived;
  `tauri-runtime-verso` is not on crates.io.
- The Rijo Box is a separate product: its in-guest browser is Servo by decision (ADR 0013),
  exposed as the `box_browser_*` tools. That is not the same surface as `browser_*`.

## Decision

**Rijo Code keeps Chromium/CDP.** There is no Servo backend, no `browser.backend` switch and
no second tool vocabulary for now. The existing `browser_*` contract stays stable.

The next work on this layer is **closing the CDP feature gap**, not changing engines:

- screenshot, `evaluate`/JS, console messages, network log, tabs, cookies,
  PDF/download, viewport control;
- the `box_browser_*` tools keep using the box's Servo daemon; they are not merged into
  `browser_*` until an active-box context exists.

**Implemented (2026-10-18):** the new `browser_dev` tool covers screenshot, PDF,
`evaluate`, console, network entries, viewport, tabs and cookies over the same CDP path —
see [`../features/browser-devtools.md`](../features/browser-devtools.md). Servo is not part
of this surface.

## Consequences

**Easier**

- Full development and research fidelity: JS-heavy pages, auth, network inspection,
  console errors, downloads and tabs behave like a real browser.
- No new engine dependency, no second build toolchain, no MPL-2.0 review for an embedded
  engine.
- One automation surface (CDP) for the core, the desktop inspector and future frontends.

**Harder / to note**

- Rijo Code depends on a Chromium/Chrome installation (or a bundled one) and inherits its
  memory footprint.
- The browser layer is not Rust-native; the "all-Rust" story applies to the harness, not to
  the web engine.
- Servo remains a strategic option for the box and a possible future backend. Revisit this
  ADR only with measured evidence (the fixture suite in the research doc), not on principle.

**Revisit when**

- Servo exposes console and network APIs (or an equivalent) and passes the fixture suite; or
- the box's Servo daemon becomes the main browser surface; or
- shipping a Chromium binary becomes a distribution problem.

## Sources / Evidence

- [`../research/2026-10-18-browser-engine.md`](../research/2026-10-18-browser-engine.md) —
  candidate matrix, measured lab results, risks.
- `crates/rijo-core/src/tools/browser_tool.rs` — current CDP implementation.
- `projects/box/repo/docs/p1-servo-spike.md` and `p1-servo-embedding.md` — what Servo can
  and cannot do today.
- ADR `0013` in the headquarters — Servo is the box's browser; this decision does not
  change it.
