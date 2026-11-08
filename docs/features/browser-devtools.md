# Browser devtools (`browser_dev`)

**Status:** ✅ implemented 2026-10-18
**Tool:** `browser_dev` — toolset `browser`, backend `browser_backend: cdp`
**Decision:** [`../decisions/0002-browser-cdp-chromium.md`](../decisions/0002-browser-cdp-chromium.md)

The five original browser tools (`navigate`, `snapshot`, `click`, `type`, `scroll`) stay
unchanged. `browser_dev` adds the surfaces a developer or researcher needs without
multiplying tool names (one schema, stable list order).

## Actions

| Action | Parameters | Result |
|---|---|---|
| `screenshot` | — | PNG under `<data_dir>/browser/`, `{path, bytes}` |
| `pdf` | — | PDF under `<data_dir>/browser/`, `{path, bytes}` |
| `evaluate` | `expression` | `{result}` — by value, promises awaited |
| `console` | `clear?` | `{entries, hooked?}` (last 200, level + text + time) |
| `network` | `clear?` | `{entries}` from the Performance API (last 200) |
| `viewport` | `width`, `height` | `{width, height}` |
| `tabs` | — | `{tabs:[{id,title,url,active}]}` |
| `tab_open` | `url?` | `{opened:id}`; the new tab becomes active |
| `tab_select` | `id` | `{selected:id}` |
| `tab_close` | `id` | `{closed:id}` |
| `cookies` | `url?` | `{cookies}` (all when no URL is given) |
| `cookie_set` | `name`, `value`, `url`, `domain?` | `{saved, success}` |
| `cookie_delete` | `name`, `url` | `{deleted}` |

**Tabs:** the other `browser_*` tools act on the **active** target. `tab_open` activates the
new tab; `tab_select` switches; `tab_close` of the active tab falls back to the first page.

## How it works

- One helper, `cdp_command`, owns the WebSocket round trip: discover the page target, send
  the command, ignore interleaved events, match the response id, 15 s timeout, surface
  `error.message`. `Runtime.evaluate` runs with `returnByValue` and `awaitPromise`, so
  numbers and objects survive (the old helper returned strings only).
- **Console** is a page-side ring buffer injected with
  `Page.addScriptToEvaluateOnNewDocument` before each `browser_navigate`. The hook patches
  `console.*`, `error` and `unhandledrejection`, keeps the last 300 entries and is
  idempotent. A one-shot CDP connection cannot listen to `Runtime.consoleAPICalled`, which
  is why the page keeps the buffer itself.
- **Network** reads `performance.getEntriesByType('resource')` — URL, initiator type,
  start, duration, transfer/encoded size. This is the Performance API, not CDP events.
- **Screenshots/PDF** are decoded from base64 and written to `<data_dir>/browser/`; the
  model can inspect the PNG with `vision_analyze` or copy it with `write_file`.

## Honest limits

- **Console logs exist only from the next navigation.** Attaching to an already-open page
  returns `hooked: false` and a note; call `browser_navigate` first.
- **Network is the Performance API,** not the CDP network stack: no headers, status codes,
  request bodies or response bodies. A `fetch`/XHR is visible, but not the response text.
- **No response body capture yet.** For a page's text use `browser_snapshot`, `evaluate`
  or `web_extract`.
- **Screenshots do not wait for a full load.** `browser_navigate` waits ~350 ms after the
  navigate response; call `evaluate("document.readyState")` or wait in JS for dynamic pages.
- **One active tab** is shared by the whole process; parallel work on two tabs is not safe.
- **PDF** uses Chromium's `Page.printToPDF` defaults (print background on, no header/footer).
- The console hook is registered once per navigation; repeated registrations are harmless
  but stay in the target for its lifetime.

## Verification

- Unit: `cargo test -p rijo-core --lib` (405 tests) and the tool contract suite (58).
- Live (ignored by default, starts a real headless Chromium and needs network):

  ```bash
  cargo test -p rijo-core --lib live_cdp -- --ignored --nocapture
  ```

  It navigates to `example.com`, evaluates `1 + 1`, captures a screenshot, lists targets and
  shuts the browser down.
