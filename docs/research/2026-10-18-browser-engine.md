# Browser engine for Rijo Code — research

**Date:** 2026-10-18

**Question:** is there a Rust browser we can integrate into Rijo Code so the agent is
"perfect" for development and research work?

> **Decision (2026-10-18):** Rijo Code keeps its `browser_*` tools on **Chromium via CDP**.
> The two-backend option below was evaluated and deferred. See
> [`../decisions/0002-browser-cdp-chromium.md`](../decisions/0002-browser-cdp-chromium.md).
> The analysis stays here as the evidence for a later revisit.

**Short answer:** **Servo** is the only serious Rust-native browser engine, and it is now a
real embeddable library (crates.io `servo` 0.7.0, LTS branch). It is **not** a drop-in
replacement for Chromium yet. The best design is therefore **two backends behind one
`browser_*` contract**: Chromium/CDP as the default for development and research fidelity,
Servo as an optional Rust-native backend. Blitz is not a browser (no JavaScript); Verso is
archived.

---

## 1. What the browser tools have to do

Rijo Code uses the browser for two different jobs, and they have different bars:

| Job | Needs |
|---|---|
| **Development** | localhost, JS-heavy SPAs, login/cookies, forms, console errors, network requests, screenshots, multiple tabs, responsive viewport |
| **Research** | real pages (docs, GitHub, papers), text/DOM extraction, PDF/downloads, pagination, cookies, proxies, stable rendering, sometimes WebGL/canvas |

The current `browser_*` tools (`crates/rijo-core/src/tools/browser_tool.rs`) are deliberately
small: raw CDP over WebSocket, own headless Chromium, five tools (navigate, text snapshot,
click, type, scroll). There is no screenshot, console, network or tab surface yet. That is
the first gap to close, independent of the engine.

---

## 2. Candidates (state on 2026-10-18)

| Candidate | Rust engine | JS | Automation | Status | License | Verdict |
|---|---|---|---|---|---|---|
| **Servo** | **yes** | SpiderMonkey | W3C WebDriver (`servodriver`) + embedding API | **active**, crate `0.7.0` (2026-10-05), LTS every 6 months | MPL-2.0 | **strategic choice** |
| **Chromium + CDP** (current) | no | V8 | CDP, the richest surface | mature | BSD-3-Clause | **default today** |
| `chromiumoxide` | client only | — | typed CDP client | active, `0.9.1` (2026-02) | MIT/Apache | optional client upgrade |
| `fantoccini` | client only | — | W3C WebDriver client | active, `0.22.1` (2026-09) | MIT/Apache | client for Servo |
| **Blitz** (Dioxus) | yes | **no** | none | beta | MIT/Apache | renderer, **not a browser** |
| **Verso** | yes | Servo | GUI browser | **archived 2025-10-08** | MIT/Apache | no |
| WebView2 / WRY | wrapper | system | limited | mature | MIT/Apache | app UI, not automation |
| CEF | no (C++) | V8 | CEF API | mature | BSD | heavy, not Rust |
| Ladybird | no (C++) | LibJS | minimal | alpha | BSD-2 | no |

### Servo facts that matter

- `servo` **0.7.0** was published to crates.io on **2026-10-05**: MPL-2.0, edition 2024,
  MSRV 1.88. Default features include `bundled` (resources baked in), `clipboard`, `js_jit`
  and `multiprocess`.
- **LTS:** a new LTS branch every **6 months**, supported for **9 months**, security fixes
  only, only the `servo` library (not servoshell). The book explicitly says Servo is 0.x and
  "production usage should be carefully evaluated".
- **Embedding docs are still sparse.** The book recommends `servoshell`, and mentions
  `tauri-runtime-verso` and `servo-gtk`. `tauri-runtime-verso` is **not on crates.io**, and
  its parent project Verso is archived — do not build on it.
- **Automation:** Servo has a real W3C WebDriver implementation (`servodriver`) with
  navigation, script execution, screenshots and input actions. It is not CDP: there is no
  network interception, console protocol, performance tracing or PDF surface.
- **Platforms:** Linux, macOS, Windows (MSVC), Android, OpenHarmony. Prebuilt nightly
  `servoshell` exists for all desktop platforms; Windows builds are **MSVC only**.

### What the lab already measured (Rijo Box)

- Prebuilt `servoshell.exe` (Windows MSVC) rendered a local page and `https://example.com`
  headless and wrote a screenshot; **~1.5 s** for the remote page.
- WebDriver end-to-end worked: create session → navigate → screenshot → `execute/sync`
  click → screenshot (page changed). **Script execution, not pointer input.**
- In-process `libservo` embedding on **Linux/WSL** is proven: offscreen rendering,
  `take_screenshot`, AccessKit a11y, `notify_input_event`; release build ~11 min cold under
  Xvfb + llvmpipe.
- A headless browser daemon on `SoftwareRenderingContext` works **without a window or Xvfb**
  (navigate, screenshot, find, click, type, scroll); it backs the `box_browser_*` tools.
- **Building Servo from source on this Windows machine fails**: GNU fails in Servo's
  `glslopt` C code, and MSVC needs VS Build Tools plus a SpiderMonkey build, which are not
  installed. A prebuilt binary is the only Windows path today.

---

## 3. Why Servo cannot replace Chromium yet

| Need | Chromium/CDP | Servo today |
|---|---|---|
| Network log / interception | yes (CDP) | no |
| Console messages | yes (CDP) | no protocol |
| Multiple tabs/windows | yes | WebDriver windows are limited |
| PDF / downloads | yes | no |
| Performance traces | yes | no |
| Modern site fidelity (WASM, WebGL, auth, anti-bot) | reference | improving, not parity |
| In-process on Windows | n/a (separate process) | **build blocked** (MSVC/VSBuildTools); prebuilt subprocess only |
| Rust-native | no | yes |

For **research** especially, network logs and console errors are often the actual finding;
giving them up would make the agent worse, not better.

---

## 4. Recommendation

**Do not replace Chromium. Add a backend, keep one tool vocabulary.**

```
browser_* tools
      │
      ├── backend "chromium"  (default)  → Chrome/Edge via CDP
      └── backend "servo"     (optional) → prebuilt servoshell via WebDriver
```

- Config: `browser.backend: chromium | servo` (default `chromium`). No new tool names, so
  the prompt cache and the model's existing knowledge stay valid.
- **Chromium path first:** close the feature gap before changing engines — screenshot,
  evaluate/JS, console, network log, tabs, cookies, PDF/download, viewport. This is what
  "perfect for development and research" needs *today*. `chromiumoxide` (0.9.1, MIT/Apache)
  is a reasonable typed client if the raw CDP client becomes unwieldy; the current raw
  client has no extra dependency, so features can also be added directly.
- **Servo path second:** pin a prebuilt `servoshell` (Windows MSVC zip, Linux tar.gz,
  macOS dmg), verify its SHA-256, store it under `~/.rijo/browsers/servo-<version>/`, start
  `servoshell --webdriver <port>` and drive it with `fantoccini` (or raw WebDriver HTTP).
  Map navigate / evaluate / screenshot / back / forward; DOM text via `execute/sync` until
  an accessibility tree is exposed.
- **Rijo Box path stays separate:** `box_browser_*` already targets the in-guest Servo
  daemon. Do not merge it into `browser_*` before the active-box context is designed.
- **Do not adopt Verso** (archived) or `tauri-runtime-verso` (not on crates.io).
- **Blitz** is useful only for a lightweight HTML/markdown preview panel; it has no JS and
  cannot browse.

### When to switch the default

Run a fixture suite on both backends and only switch when Servo wins or ties on the
development/research set:

| Fixture | Checks |
|---|---|
| `docs.rs`, MDN, Wikipedia | text extraction, navigation, latency |
| GitHub repo/PR pages | auth, DOM, forms |
| a JS SPA (e.g. a local Vite app) | console, routing, clicks |
| a canvas/WebGL demo | rendering fidelity |
| a login flow | cookies, redirects, storage |
| a PDF/download page | download handling |

Metrics: task success, wall time, memory, binary/download size, failure reasons. No default
change without this evidence.

---

## 5. Phased plan (estimates, not commitments)

| Phase | Work | Estimate |
|---|---|---|
| **P0** | Backend seam: `browser.backend` config, one internal trait, existing CDP code behind it; tools unchanged | 1–2 days |
| **P1** | Close the Chromium feature gap: screenshot, evaluate, console, network log, tabs, cookies, PDF/download, viewport | 2–3 days |
| **P2** | Servo backend: pinned download + checksum, `servoshell --webdriver` lifecycle, `fantoccini`, navigate/evaluate/screenshot | 3–5 days |
| **P3** | Fixture suite + measured comparison; decide the default | 1–2 days |
| **P4** | In-process `servo` for the box / future desktop; revisit when Servo reaches 1.0 or an LTS fits | later |

P2 runs on Windows with the **prebuilt MSVC** binary; no build toolchain is needed. The
crate `servo` is the option for in-process embedding on Linux/macOS, not for this Windows
host today.

---

## 6. Risks and open questions

- **MPL-2.0:** file-level copyleft. Embedding the library is fine; modifications to Servo
  files must be published. This needs a legal check before shipping a modified engine.
- **0.x churn:** pin a version; LTS every 6 months, 9 months support. Expect migration work.
- **Prebuilt trust:** a downloaded browser binary is a supply-chain surface. Pin versions,
  verify SHA-256, and keep the download optional (no silent network fetch).
- **Windows in-process embedding** needs MSVC + VS Build Tools + SpiderMonkey; a subprocess
  is the pragmatic path.
- **Servo WebDriver gaps** (console/network/tabs) decide whether it can ever be the *only*
  backend for research.
- **Does the agent need network interception?** If yes, Chromium remains mandatory for that
  class of tasks.

---

## 7. Sources

- Servo repository and license: <https://github.com/servo/servo> (MPL-2.0, active)
- Servo crate 0.7.0: <https://crates.io/crates/servo> (published 2026-10-05)
- Servo Book, embedding overview: <https://book.servo.org/embedding/overview.html>
- Servo Book, LTS releases: <https://book.servo.org/embedding/lts-release.html>
- Servo Book, WebDriver/servodriver: <https://book.servo.org/design-documentation/servodriver.html>
- Servo downloads (nightly, all desktop platforms): <https://servo.org/download/>
- Verso (archived 2025-10-08): <https://github.com/versotile-org/verso>
- Blitz (beta, no JS): <https://github.com/DioxusLabs/blitz>
- `chromiumoxide` 0.9.1: <https://crates.io/crates/chromiumoxide>
- `fantoccini` 0.22.1: <https://crates.io/crates/fantoccini>
- Lab evidence: `projects/box/repo/docs/p1-servo-spike.md`,
  `projects/box/repo/docs/p1-servo-embedding.md`,
  `docs/20-projects/rijo-box.md` (measured 2026-10-05 … 2026-10-11)
