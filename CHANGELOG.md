# Changelog

All notable changes to Rijo Core. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows
[SemVer](https://semver.org/).

> Entries before 2026-11-08 describe the Rijo Code project **before** the repository split;
> they include the desktop workbench, which now lives in the private `rijoda/rijo-code`
> repository. They are kept as history, not as a description of this open core.

## [Unreleased]

### Added

- **`browser_dev`** — the CDP surfaces the browser tools lacked: PNG screenshot, PDF,
  `evaluate` JS by value, console ring buffer, network entries, viewport override, tabs,
  cookies. One tool with an action enum, so the schema list stays stable.
- **`rijo-desktop-dev.exe`** — one executable for the dev loop: builds the dev binary,
  starts Vite (hot reload) or reuses a running one, waits for port 1420, opens the window
  and stops Vite when it closes. `--skip-build`, `--port`, `--no-cdp`.
- **`rijo-desktop`** — the Tauri 2 workbench: projects, session tabs, live inspector,
  approvals and questions over the shared `ui_events` channel.
- **Desktop attachments** — native file picker (`Ctrl+Shift+A`), chips in the composer,
  text inlined and images as `image_url` parts; a bad file aborts the send instead of
  disappearing.
- **`git` and `brain_outline` tools** — 33 registered tools in total.
- **Shared UI contract** — `rijo_core::ui_events` and `rijo_core::inspect`; the TUI and the
  desktop both consume them.

### Fixed

- **`router.max_tools` was silently ignored** — the router always used the ONNX bundle's
  cap. `RouterSession::set_max_tools` now applies the config value (default 6); the
  always-keep tools are added after the cap.
- **Tighter context defaults** — `tool_result_cap` 8000 → 6000 chars,
  `tool_stub_after_messages` 8 → 6. Measured A/B (same prompt, one run per arm):
  182k → 73k billed prompt tokens, but the model path also changed (27 → 12 tools); the
  deterministic effect is a ~8 % smaller schema and a lower per-result worst case. Details:
  [`docs/research/2026-10-18-context-budget-ab.md`](docs/research/2026-10-18-context-budget-ab.md).
- **The context meter showed billed tokens as context.** A 22-iteration website audit
  reported 335k "context" tokens although its last request was ~6k; the router had trimmed
  the schemas (20k → 8k chars) and pruning kept the request at 5–8k. `TurnStats` now carries
  `last_prompt_tokens`; the desktop meter/footer and the TUI gauge use it, the cumulative
  sum stays visible as "billed".

### Changed

- **Questions are an inline card in the conversation**, not a sidebar list: one question at
  a time with progress, selectable option rows (`1`–`9`), a `Recommended` chip, free text,
  Back/Skip and Enter to advance. Pending questions are stored with their payload and
  replayed from the bootstrap, so a reload cannot strand a waiting turn.
- **Tool targets are summarized in the core** — `summarize::tool_call` turns the arguments
  into one human line (`terminal · cargo`, `read_file · lib.rs`). The executor emits it as
  `ToolCall.args_summary`; the TUI and the desktop render the same wording instead of raw
  JSON. Unknown tools fall back to the bounded raw arguments.
- **Activity inspector** — a per-turn tool timeline: arguments and results on demand,
  honest `WORKING`/`IDLE`, failure badges, human durations, its own scroll area and a
  "new activity" button; five turns of history in memory.
- **Documentation structure** — the README is a map again. The chronological record, the
  German goal document and the old TUI docs moved to `docs/history/`; the current picture is
  `docs/status.md`.
- **TUI frozen** — bug fixes only; it is rebuilt after the desktop covers the remaining
  workflows.
- **Security hardening** — `.env`/secret variants are refused by every file tool, `execute_code`
  has a timeout and capped output, MCP requests are bounded, sync sanitises host labels.

## [0.1.0] - 2026-10-11

### Changed

- **`web_search` reduced to the built-in keyless DuckDuckGo.** The multi-provider
  cascade, RRF fusion, the local reranker, Exa/SearXNG/Firecrawl/Wikipedia and the
  `--web-eval` harness were removed; a dedicated search engine is planned
  separately (see `docs/history/goal-100-de.md` §AK).

### Added

- **Headless multi-turn sessions (`rijo --repl`).** Reads prompts from stdin (one
  per line) and runs one turn each, **all in one process** — so the provider's
  prompt cache applies across turns instead of starting cold every turn. This is
  what makes the router on/off measurement honest: in a 6-turn session the router
  halves the prompt-token volume (**111 737 → 55 737, −50 %**; see
  `docs/history/goal-100-de.md` §Y.2). Also handy for scripted batch runs.
- **Context compression — the model-free planning core.** `crates/rijo-core/src/compress.rs`
  holds the part of history summarization that needs no model and no database:
  splitting a history into *to-summarize* + the newest `keep_recent` verbatim
  (`plan`), the summarization prompt (`summary_prompt`), the answer cap
  (`parse_summary`), the folded history entry carrying its watermark
  (`summary_message`), the trigger threshold with its cache-stability guards
  (`should_compress`) and the minimum-saving check (`worth_it`). It is
  **incremental by construction**: a leading `[history summary …]` message is
  merged, never re-summarized, so the next run is O(new) rather than O(all).
  `summarize` is the thin model call on top (the answer is trimmed and capped; an
  empty answer is an error, so a bad call cannot wipe the history). 8 unit + 2
  contract tests (the model is replayed via `RIJO_LLM_MOCK` in a dedicated test
  binary); the executor trigger is the next step.
- **Execution sandbox seam (ADR 0018).** `terminal` and `execute_code` now ask
  `sandbox.backend` *where* their process may run: `off` (default, unchanged),
  `auto`/`bwrap` (wrap in **bubblewrap** on Linux — read-only system, the
  workspace writable, network optional), or `box` (execute inside a **Rijo Box**).
  The box client is not wired yet, so selecting `box` **fails closed** — it never
  silently runs on the host (an integration test pins exactly this). Tool names
  and schemas are unchanged, so the prompt-cache invariant holds; the box slots
  in without touching the tool layer. Default is `off` on purpose: the wrapper
  changes every command and is opt-in until measured. Background execution under
  a non-`off` backend is refused with a clear message, not silently downgraded.
  The guard still decides *whether* to ask the user; the backend decides *what an
  allowed process may touch*.
- **MCP client (`mcp` tool, ADR 0017).** Rijo Code can now *consume* external
  Model Context Protocol servers, the counterpart of the `rijo mcp serve` it
  already exposes. Configured under `mcp.servers` (stdio child processes); the
  agent uses one stable tool with `list_servers` / `list_tools` / `call`, so the
  tool schema — and with it the alphabetical order and the 94 % prompt cache —
  does not change. A remote tool has no guard of its own, so only names listed
  in a server's `read_only_tools` run without a dialog; every other call goes
  through the same guard as `terminal` (TUI confirmation, denied by default
  headless), and an undeclared tool is denied before a process is spawned.
  Built on `rmcp`'s `client` + `transport-child-process`; verified against
  Rijo's own server (`crates/rijo-cli/tests/mcp_{client,tool}_contracts.rs`,
  6 green). Details and open points (`docs/MCP.md`): pooling, HTTP transport,
  named tools. In the CLI, `rijo mcp list [server]` shows the configured servers
  or one server's tools without a model turn.
- **Attach files and images to a message** (`/attach <path>`, `/detach`). The
  attachments are listed above the input until you send. On Enter the bytes travel
  WITH the turn — text files are inlined (capped at 20k chars, marked when cut) and
  images become real `image_url` parts in the request, so a vision model sees the
  picture with **no extra round-trip**. A model that cannot take images (e.g.
  `deepseek-chat`) gets a named note instead of a provider-side 400;
  `RIJO_VISION=on|off` overrides the capability guess. Details and its honest
  limits: `docs/history/tui/attachments.md`. Side effect: `vision_analyze` now
  shares `LlmClient::describe_image` and therefore carries the provider's session
  header — it used to build its own request and would have failed on OpenCode with
  HTTP 400 `MissingSessionID`.
- **Paste an image straight out of the clipboard** (`Ctrl+V`, `/paste`). A copied
  screenshot becomes an attachment; a copied image **file** (a path from Explorer
  or a drag & drop) is attached too, instead of being typed into the prompt. rijo
  asks the OS for the image itself (PowerShell `GetImage()` on Windows,
  `wl-paste`/`xclip`, `pngpaste`) on a worker thread, scales the long edge to
  1568 px and writes a PNG into the temp dir. Text pastes as before; an empty
  clipboard only toasts. **Windows Terminal binds Ctrl+V itself and never sends
  the key** (microsoft/terminal#19517, open) — rijo says so once at startup and
  `/paste` is the reliable path there; a plain console or mintty passes Ctrl+V
  through.
- **`arxiv` research tool** (30th tool): search arXiv, fetch paper metadata and
  abstracts by id, and pull citation stats, references and citing papers from
  Semantic Scholar — no API key. The Atom XML is parsed in Rust (`quick-xml`),
  results are cached 24 h in the same `brain` store `web_search` uses, and the
  documented rate limits (one request per 3 s at arXiv, 1/s at Semantic Scholar)
  are respected by a queued throttle instead of a burst. Plain-text queries are
  prefixed with `all:`; arXiv's own syntax (`au:`, `ti:`, `cat:`, `AND`/`OR`/
  `ANDNOT`, `"phrases"`) passes through. A withdrawn paper is flagged.
- **MCP server** (`rijo mcp serve`): the 31 native tools over stdio for any MCP client, exposed as
  two meta-tools (`list_tools`, `call_tool`) that route through the same Guard as the TUI. The
  server is headless, so dangerous calls are denied by default unless the process runs with
  `RIJO_APPROVE_DANGEROUS=1`. Verified with a real handshake; see `docs/MCP.md`.
- **Provider templates** (`--provider`, `--model`, `--base-url`,
  `--list-providers`). The first template is **OpenCode Zen/Go**, which makes the
  free development models usable for Rijo Code itself:
  `rijo --provider opencode --model space-bunny-free`. The template encodes the
  three requirements the live API turned out to have — base URL
  `https://opencode.ai/zen/go/v1` (the `api.`/`models.opencode.ai` hosts answer
  Cloudflare 1010), a mandatory `x-opencode-session` header (otherwise HTTP 400
  `MissingSessionID`), and a browser-like `User-Agent` (the edge blocks the
  default reqwest agent). `LlmConfig` gained `session_header` and
  `extra_headers`; streaming, tool calls and `reasoning_content` were probed and
  work unchanged.
- **Token resolution without copying secrets**: a provider's own environment
  variable, then its own credential file
  (`~/.local/share/opencode/auth.json`), same pattern as `gh auth token`. A key
  from the config is used only when it belongs to the same provider — reusing a
  foreign key produced a real HTTP 401 in the first live run.
- 16 unit + 10 integration tests for the provider layer
  (`crates/rijo-core/src/providers.rs`, `tests/provider_contracts.rs`), including
  "a key for another provider is not reused", "a chosen model survives applying a
  template" and "neither the note nor `--list-providers` ever contains the token".
- **DeepSeek is a first-class provider.** `--provider deepseek` points at
  `https://api.deepseek.com/v1`, takes the key from `DEEPSEEK_API_KEY` (no CLI, so
  no credential file) and defaults to `deepseek-chat`:
  `DEEPSEEK_API_KEY=... rijo --provider deepseek`. No session header and no extra
  headers — unlike OpenCode, DeepSeek needs neither. `--provider ds` works as an
  alias, a bare `--base-url https://api.deepseek.com/v1` is recognised, and the
  onboarding offers DeepSeek first. New `ProviderTemplate.default_model`: a
  template may now name a model, and `apply()` uses it **only** when the caller
  never chose one (the built-in default is an OpenAI model and cannot work against
  another endpoint; a model you set is never touched).
- The onboarding's remaining German strings are English now (`Choice`, `API key`,
  `Validating`, `failed`, `network`, `key rejected`, `100+ open models`). They were
  missed by the `i18n: complete runtime interface to English` pass and contradict
  the convention that everything a product ships is English.
- **Tool router (off by default, milestone M0–M4).** A local multilingual
  DistilBERT scores the user prompt and the executor sends only the tool schemas
  above their calibrated threshold — the ~10k characters of tool schemas are the
  largest fixed part of every prompt. The full tool list is always the fallback:
  a missing or broken model may cost time, never behaviour. `RouterSession`
  (tokenizer + ONNX via `ort`) in `AppState.router`; the filter keeps the
  alphabetical order so the prompt prefix stays byte-identical. Every turn
  writes a `decision_trace.jsonl` line (prompt, tools sent, tools chosen,
  full vs. sent characters, fallback, router_ms) — that trace is the training
  data for the retrain. `heads.guard` and `heads.compact` stay `false` until
  their gates have enough data; see `docs/history/goal-100-de.md` §Y for the numbers, the
  dataset and the open data gap on the guard head.
- Training pipeline and dataset under `scripts/router/` and `data/router/`
  (5 390 rows over all 29 tools, family-wise split, holdout leakage check
  passes). The trained checkpoints are not in the repo (`/runs/`, 1.6 GB); the
  runtime bundle is the ONNX export in `~/.rijo/models/router/v1/`.
- **Windows support.** `install.ps1` (no admin, installs to
  `%LOCALAPPDATA%\Programs\rijo`, places `onnxruntime.dll`, sets the user PATH).
  The build is not Linux-only any more: the ONNX loading strategy is per target —
  `download-binaries` on Unix, `load-dynamic` on Windows, because `ort-sys` ships
  no prebuilt for `x86_64-pc-windows-gnu`. Measured: 239 tests green on Windows,
  Chrome headless starts with the tool's own arguments.

### Fixed

- **The tool router is now usable and measured.** Retrained for the 31-tool
  registry on **XLM-R base** (was DistilBERT): tools mAP8 **0.897 mean / 0.920
  best** (validation, 3 seeds) after regularisation (dropout 0.3, label smoothing
  0.1, weight decay 0.05, 12 epochs). The E2 A/B (33 tasks x 3 = 99 runs, DeepSeek) shows the
  router sends **94 % fewer schema characters** (17 321 → 1 076 per run) **and**
  picks the right tool **89/99 (90 %)** (baseline 78/99 = 79 %) with 0 fallbacks, no net latency cost (median 1230 vs 1304 ms; the smallest prompt offset), at ~0.2 s local
  routing per turn. A real-turn A/B (same task, router off vs on) shows 14371 vs
  5248 prompt tokens (9123 fewer, -64%). `router.enabled: true`; the bundle, checkpoints and dataset
  are published at `huggingface.co/slomo-b/rijo-router` (private) and mirrored
  locally.
- **The router training script never called `opt.zero_grad()`** — gradients summed
  over every batch, so the loss diverged inside the first epoch and the tool head
  collapsed (measured: 1.5 → 15 → 22, mAP8 collapsed). Fixed; a full fine-tune
  (6 epochs, 3 seeds) now reaches **tools mAP8 0.70–0.74** on the 31-tool dataset.
  The router dataset also gained `mcp`/`arxiv` (was 0 and 4 rows), and the router
  toolchain now reads/writes files as UTF-8 (cp1252 crash on Windows). The runtime
  bundle is built and verified — `scores_with_real_bundle` (previously `#[ignore]`)
  is green: **30 ms median** scoring, routing a read prompt to `read_file` (0.986).
- **Every write was rejected on Windows.** `is_in_workspace` compared a
  `canonicalize()`d path (`\\?\C:\...`) with a lexically built workspace
  (`C:\...`); `Path::starts_with` compares components, so the prefix never
  matched. That affected `write_file`, `patch` and `delete_file` — 20 of 27
  integration tests. The verbatim prefix is stripped now and the comparison is
  case-insensitive on Windows.
- **23 sites read `HOME` with a `/home/user` fallback.** Windows sets no `HOME`,
  so the tool trace, the guard audit log, the trust store, the update staging,
  the provider token, the Chrome profile, the TTS voices and both model caches
  landed in `C:\home\user\.blaze`. All of them go through `paths.rs` now.
- **`execute_code` was dead on Windows** (hardcoded `python3`); the interpreter is
  probed once (`python3` → `python` → `py`).
- **A timeout killed nothing on Windows**: `kill(-pid)` was `#[cfg(unix)]` and the
  child had already moved into the waiting thread. `process::kill_tree()` is
  `killpg` on Unix and `taskkill /T /F` on Windows.
- **`which` does not exist on Windows**, so the Chromium and TTS probes found
  nothing even with the program installed; `paths::which_bin()` reads `PATH` and
  `PATHEXT` instead, which also removes a process spawn per probe. Chromium is now
  additionally looked up under `C:\Program Files\...`, where Chrome and Edge
  actually install.
- **`shell_command()` returned the shell path with quotes**, which `Command::new`
  reads as part of the program name (`os error 123`, ERROR_INVALID_NAME).
- **`sync.rs` hostnames** fell back to `unknown-host` off Linux, so two Windows
  hosts would share a name in the sync manifest.
- **`trust_marker()` flattened only `/`**, but `\` and `:` are illegal in a
  Windows file name — the marker could not be written.
- Two integration tests were flaky: they shared one `brain_test.db` across
  parallel tests, and the latency guard compared a single timing sample.
- **The onboarding's live key check crashed instead of validating.** It used a
  blocking HTTP client while `main` is a tokio runtime, which panics with
  "Cannot drop a runtime in a context where blocking is not allowed". The wizard's
  headline promise ("validate the API key, done") had never once worked, and no
  test covered it because it needs a network call. It now runs on its own thread.
  Measured against `api.deepseek.com` with a deliberately invalid key:
  `[401 Unauthorized] key rejected`, three attempts, clean abort, no panic.
- **The wizard could write a config the API rejects.** Choosing `opencode-go`
  wrote the base URL but not `x-opencode-session`, so the first turn answered
  HTTP 400 `MissingSessionID`. It writes the template's headers now — base URL and
  headers come from one place, and a test pins that a menu choice cannot produce a
  config the endpoint refuses.
- The wizard asked for a **workspace** and threw the answer away (`let _workspace`).
  The question is gone; the workspace is the directory `rijo` runs in.
- The key check ran against a model guessed from the base URL. It now uses the
  model the user actually chose, and asks for the model before the key.
- **Websurfx could never install.** `git clone <url> .` refuses a directory that
  is not empty, but the code wrote its build lock into the clone target *before*
  cloning — and its failure marker *after*. Measured: with only
  `.rijo-build-failed` present, the clone still exited 128 ("destination path '.'
  already exists and is not an empty directory"), so the checkout never appeared
  and search fell back to Wikipedia forever. The lock now lives beside the
  checkout (`~/.rijo/.websurfx-build.lock`) and the directory is cleared before a
  clone; the reason for a failed clone is kept in the log instead of being
  discarded.
- **The Websurfx clone/build wrote straight to the terminal from a background
  thread** (`eprintln!`, and `cargo build` with inherited stderr — hundreds of
  compiler lines). While the TUI owns the terminal that desynchronises ratatui's
  buffer, which is exactly the reported symptom: the clone/build text sitting on
  screen and typed input landing somewhere else. All of it now goes to
  `<data>/websurfx-build.log`. (`debug::note` enforces the same rule one layer
  up; a test pins that the lock and log stay out of the clone target.)
- **The input row could show leftover text from an output.** A write to the
  terminal from a background thread (`eprintln!`, or an inherited `cargo build`
  stderr) moves the cursor behind ratatui's back; ratatui then treats those cells
  as unchanged and the stray line stays on screen — on the input row in the
  report. The Websurfx writer is gone (above); every other spawn was audited and
  already redirects its output (`process_manage`/diagnostics to files, TTS to
  pipes, the browser and language servers to null).
- **`render_input` could panic on non-ASCII input.** The cursor character was
  carved out with `rest.split_at(1)` — a BYTE index — so pressing `Home` before an
  "ü" aborted with "byte index 1 is not a char boundary". It works in chars now.
- **Every typed character appeared twice on Windows.** crossterm reports a
  `KeyEventKind::Release` for each physical key press; the key reader forwarded
  it, and the handler inserts on any key event, so a press+release pair inserted
  the character twice. Release events are dropped now — `Repeat` is kept, so
  holding a key still repeats.
- **`Ctrl+↑/↓` to scroll never ran.** The unguarded `KeyCode::Up`/`Down` arms sat
  above the modifier guards in the same `match` and matched first; the modifier
  arms are ordered before them now.
- **Scrolling never reached the chat.** `↑`/`↓` always walked the input history,
  and some Windows terminals report the mouse wheel as `↑`/`↓`. With an empty
  input the arrows scroll the chat now; with text typed (or a recall in progress)
  they still walk the history.
- **Copying text out of the chat produced mojibake on Windows.** The selection
  crate pipes the copied text into `clip.exe`, which decodes stdin in the *console
  code page* — CP850 here — not UTF-8: a 9-character string with `ü ß · →` came
  back as 14 garbage characters, which is what the report described. Rijo Code
  turns that backend off on Windows and copies through PowerShell with an explicit
  UTF-8 stdin read (`Set-Clipboard`), verified to round-trip the text exactly;
  OSC 52 stays the fallback and the Unix path keeps the crate's xclip/wl-copy.
- **Pasting into the input crawled in "as if it were being typed".** Bracketed
  paste was never enabled, so the terminal delivered a pasted block as one key
  event per character and the TUI redrew for each one. Bracketed paste is on now:
  a paste arrives as a single event and is inserted in one edit. Line breaks and
  tabs become spaces and other control characters are dropped, so a paste cannot
  smuggle an escape sequence into the buffer. A paste into the open command
  palette or file picker goes to that overlay's filter field.
- **A pasted prompt was split, and part of it sent immediately.** crossterm 0.29
  implements `Event::Paste` only in its *Unix* parser — on Windows a paste arrives
  as a burst of key events, and every line break in it reached the handler as
  Enter, which SENT the prompt mid-paste; the rest stayed in the input. The key
  reader now coalesces a burst of plain characters into one paste event (a burst
  Enter becomes a newline, a lone Enter still sends), and it forwards a single
  character as a key event so the `@` picker and the `/` palette keep opening.
  That also removes the redraw-per-character slowness on Windows, because the
  burst now arrives as one event. On Unix a pasted line break in raw mode is
  Ctrl+J (crossterm #371) and is handled as a newline too.
- *(correction to the entry above)* Coalescing in the key reader added a small
  delay to every keystroke and still relied on a timing window too tight for
  Windows' key delivery, so a paste could still be split. The reader now forwards
  every character immediately and redirects only the Enter: a line break belongs
  to a paste when a character arrived within 50 ms before it **or** more input is
  already waiting behind it (80 ms lookahead). `enter_is_pasted_newline` is that
  rule, and it is unit-tested. The redraw-per-character cost is handled by
  skipping the frame draw while input is queued — typing adds no latency and a
  paste no longer repaints hundreds of times.

### Changed

- **Progress and re-read guards (from a real static-site run).** Two failure modes
  that run showed are now bounded. (1) After **ten** tool calls in a row that change
  no file, the tool result carries a **progress nudge** — "act now (patch/write_file)
  or answer; one targeted search instead of many" — a nudge, never a hard stop; a
  `patch`/`write_file`/`delete_file` resets the streak. (2) A **full re-read** of a
  file already read this session returns a **stub** (path, size, note) instead of
  re-sending the content; a paged read (offset/limit) still returns content, so
  verification keeps working. Both rules are pure and unit-tested.
- **The status area above the input, modernised and clickable.** The context fill
  and the cache hit now carry a five-cell gauge (`ctx 44k/128k 35% ▰▰▱▱▱`), so
  "how full" reads at a glance; the segment's colour still tells the same story
  (dim/amber/red). A **left click** on the activity row toggles the full breakdown
  (same as Ctrl+O), a click on the todo strip expands the checklist (same as
  Ctrl+T) — a move or drag never toggles (`toggle_status_row`, unit-tested).
- **Workflow steering against the observed detours.** A real run re-audited facts
  it already had and used an interpreter plus scratch `.txt` dumps for file work
  that `read_file`/`search_files`/`patch` do directly. The system prompt now states
  three hard rules — no interpreter for file work, no scratch dumps, implement
  instead of re-auditing — and `terminal` appends a **soft hint** (never a block)
  when a command runs `python`/`node` file I/O, so a legitimate script still runs
  and the model is told at the moment it happens.
- **Zero compiler warnings.** The workspace built with 45 warnings; all are
  resolved and `cargo build --workspace --all-targets` is warning-free. Most were
  machine-applicable (16 via `cargo fix`: unused imports, a redundant `;`, unused
  `mut`/variables); the rest by removing dead code (`AsyncBridge`, `jobs_dir`,
  `last_modified`, `strip_tags_and_extract_text`, `short_reasoning`, the unused
  `CANCEL` static, `DEFAULT_CACHE_TTL`, `POLL`, two never-read TUI fields, the
  never-read `Response.remaining`) and migrating two deprecations
  (`rand::thread_rng` → `rng`, `quick_xml` `unescape_value` → `normalized_value`).
  Two real bugs fell out: the **`Alt+Home`/`Alt+End`** chat-jump arms sat *below*
  the unguarded `Home`/`End` arms and were therefore unreachable (the same
  shadowing the Ctrl+arrow comment already described) — now ordered correctly;
  and `render_frame_text` was `pub` while its `App` argument was private. The
  genuinely parked helpers (`render_minimap`/`total_wrapped_lines`,
  `short_reasoning`) keep a scoped `#[allow(dead_code)]` with a note. No behaviour
  change otherwise.
- **The onboarding is down to at most three questions, and to none when a key is
  already there.** It looks for a provider whose key is available (its own
  environment variable, or its CLI's credential file) and skips the provider
  question entirely, saying what it found and how to override it. A key already in
  the environment is offered as the prompt default, so Enter is enough.
  `ProviderTemplate.default_model` supplies the model default (DeepSeek →
  `deepseek-chat`, OpenCode → `space-bunny-free`), and the closing line prints the
  endpoint plus a command to try.
- **Web search is a pluggable chain instead of one self-built daemon** (ADR 0007):
  a keyed API (Brave `BRAVE_API_KEY`, Tavily `TAVILY_API_KEY`) → `WEBSURFX_URL` if
  one is configured → **Firecrawl keyless** → Wikipedia. It works with no key at
  all (Firecrawl answers out of the box; an optional `FIRECRAWL_API_KEY` only
  raises its limits) and every step has the same bounded timeout plus its own
  cooldown, so one dead provider no longer costs its timeout on every search.
  `websurfx_timeout_ms`/`websurfx_cooldown_secs` became `search_timeout_ms`/
  `search_cooldown_secs` (old names still deserialize). The endpoint of each API
  is overridable (`web.brave_api`, `web.tavily_api`, `web.firecrawl_api`).
- **The Websurfx clone/build no longer runs by default.** It took minutes, the
  daemon needs Redis, and on Windows its progress went into the TUI. A running
  instance is still adopted when `WEBSURFX_URL` is set; building one is opt-in via
  `web.autostart_websurfx: true`.
  Measured on this machine (2026-10-04, median of 3): Wikipedia 0.40 s, Firecrawl
  1.2 s warm (first cold call 2.0 s). Public SearXNG was unusable — 0 of 7
  instances returned JSON (3 answered HTML at HTTP 200, 3 answered 429 at once).
- **The input box is cleaner and scrolls.** The redundant "Input" title is gone —
  the `❯` prompt already says where typing happens; the title now appears only
  while a turn runs (the input is locked then). The prompt has padding after the
  border, and long input scrolls horizontally so the cursor stays visible instead
  of running into the border.
- **The status rows read consistently.** Every value after the first is introduced
  by exactly one ` · `. `ctx … % ttft …` and `ready bg 1` were missing the
  separator, and a shortened row ended on a dangling ` · …`. Truncation now drops
  whole values and ends on ` …` (a half-written number is still never shown), and
  the tool excerpt separator uses the same single space as everywhere else
  (`412ms │ started`).
- **The status row is down to the values worth a permanent glance** — context
  fill, TTFT, turn time, cache ratio (and failures, which must never hide). The
  full breakdown (LLM/tool split, iteration and tool counts, throughput, brain
  hits, session totals) is one `Ctrl+O` away, the same toggle that expands a tool
  call. Eleven values at once buried the four that matter.
- **A coherent, muted colour palette in one place** (`theme`). The saturated
  16-colour primaries gave way to a softer set; the assistant role got its own
  teal instead of yellow (which reads as a warning), the dim tone is brighter and
  more readable, and the section-header rule is its own token (`RULE`) rather than
  a scattered literal. Every stray `Color::` literal now routes through the theme.
- **Error entries no longer show the mark twice.** A failed tool already carries
  "✗ read_file … │ reason"; the "✗ error" section header is skipped for inline
  results.

- **The input box grows and scrolls.** A long or multi-line prompt is wrapped
  per character and the box grows with it (up to 8 lines), then scrolls to keep
  the cursor visible — before, the input was a single line that scrolled
  horizontally, so a pasted prompt was mostly off-screen. Enter still sends;
  Alt/Shift/Ctrl+Enter inserts a line break. A paste keeps its line structure. The
  input must never be cut off or reflowed: the wrap is per character, not per word.
- **The transcript now shows where every line comes from.** A two-column origin
  marker (`● you`, `◆ rijo`, `⟡ thinking`, `⚙ tool`, a tool result's own `✓`/`✗`,
  `? frage`, `✔ todos`, `✗ error`, `· status`) and role-coloured header rules
  replace the neutral grey rule. A tool result's mark is promoted into the marker
  column, so it is never doubled, and the column keeps the same width for every
  role. The new `Question` and `Todo` roles are the base for the interactive
  question overlay and the todo pane.
- **A todo strip above the input shows where the work stands.** It reserves rows
  only while a list exists: one line with progress and the task being worked on
  (`✔ 1/4  ▸ Fehler isolieren  · 2 offen`), expanded with `Ctrl+T` to one line per
  todo (`☐ ▸ ✔ ✗`). The `todo_list` tool refreshes it live (`TodosChanged`),
  `/todos` prints the list, and the file survives a restart.
- **`clarify` is interactive now.** Instead of echoing JSON and waiting for the
  next user message, it opens a modal, accented question overlay and the turn
  **blocks** until the answer: `↑/↓` or `1-9` pick an option, `Enter` confirms and
  moves on, `Tab` switches question, `Esc` skips, and the last row is free text.
  Up to 5 questions with up to 6 options each, the recommended one highlighted.
  Headless runs (`--query`, `mcp serve`, tests) keep the old data path, so nothing
  hangs where there is no UI.
- **The system prompt asks for a todo list on multi-step work** and for `clarify`
  when a decision is genuinely missing. The visible todo strip and the question
  overlay are that rule's UI; a one-step task still needs neither.
- **A tool result sits under its call.** The entry used to be one long line
  (`… 412ms │ started · proc_1`); it is now split — the call keeps the origin
  marker, the result moves to its own indented line behind a `│` edge. Hierarchy
  is visible instead of implied by a separator.
- **Eight fixes from a screenshot review.** The collapsed reasoning line no longer
  repeats the role (`⟡ thinking` header above a bare `▸ 9 words` line); the
  **current** reasoning streams expanded (watching it think is the point) while a
  **finished** block collapses to one line, so the transcript is not left full of
  old thinking; thinking is dimmed so the answer stands out;
  the new UI strings are English again (the product-language rule — `? question`,
  `Other …`, `open`, `answered:`); the live-phase separator matches the tool lines
  (`·`, not `—`); the data row carries a second live value (`· N msg`) instead of
  the estimate alone; the todo strip's `Ctrl+T` hint is right-aligned; and
  `todo_list`'s result reads `N tasks · M done · K running` instead of the
  confusing "3 entries" / "6 task(s)" pair.
- **Three transcript fixes from a real session.** `⚙ ⚡ brain` carried two glyphs
  (the row's origin marker plus the text's own `⚡`); the brain line now starts
  with `brain · …`. A `terminal` row whose command was `cd <path> && real-command`
  showed the useless keyword `cd` — the real command is shown now. And thinking
  blocks no longer draw a full-width rule each; a short tail keeps the marker
  without turning the transcript into a wall of rules (25 such blocks in one
  session).
- **A large file write hit the output cap and arrived cut off.** The harness
  already raises DeepSeek to its documented 8192-token output cap, so the old
  advice ("raise llm.max_tokens") was a dead end. The error now says what actually
  works — write the file in pieces (`write_file` first, then `patch`) — and the
  system prompt states the ~150-line threshold explicitly, because "huge arguments
  can get truncated" alone was not concrete enough to stop it.
## [0.1.0] — 2026-09-15

First public release: a native Rust harness for agentic coding that executes the
tool layer without a Python interpreter (measured 13x–1000x faster per tool call
than the Python reference, `rijo --bench`).

### Included

- **27 tools** — `terminal`, `read_file`, `write_file`, `patch`, `search_files`,
  `delete_file`, `brain_search` (retrieval over the indexed workspace),
  skills (`skills_list`, `skill_view`, `skill_manage`), `memory`,
  `execute_code`, `todo_list`, `clarify`, `process_manage`, `web_search`,
  `web_extract`, browser tools (CDP), `vision_analyze`, `text_to_speech`,
  `session_search`, `delegate_task`, `cronjob_manage` (`rijo --tools`).
- **TUI** with a live tool feed, thinking display, session history (`--resume`)
  and process/metric overlays.
- **Background processes**: `terminal(background: true)` puts the process into a
  registry (logs go to files, `process_manage` offers poll/log/wait/kill)
  instead of blocking the turn; timeouts kill the whole process group.
- **Guard**: destructive actions are intercepted on every execution path,
  approval only through a TUI dialog (headless fail-closed), audit trail in
  `~/.rijo/data/guard_trace.jsonl`. Honest limit: heuristic, not a sandbox
  (see `SECURITY.md`).
- **Learning review after the turn**: one cheap LLM call decides whether the
  turn produced a lesson for a skill or for memory
  (`SKILL_CREATE`/`SKILL_UPDATE`/`MEMORY`), writes only inside `~/.rijo/` and
  logs its cost to `turn_metrics.jsonl` (`kind: "learn"`).
- **Skill index in the system prompt**: skills (name + short description, max
  40, own ones first) sit at the end of the system prompt so the model knows
  them without a tool call — previously learned skills sat on disk, invisible.
- **Parallel batches** for read-only tool calls (3.8x on 8 calls, measured).
- **Prompt-cache fix**: deterministic tool ordering lifts the cache hit rate
  from 26 % to 94 % (measured over 3967 tokens/turn).
- **Terminal latency**: the 50 ms poll sleep is gone, output is collected by
  reader threads (50.9 ms → 2.0 ms per short command, measured; this also fixed
  a deadlock for output larger than the pipe buffer).
- **Self-diagnostics**: `--bench` (Rijo Code vs. Hermes comparison), `--schema-eval`
  (tool selection on a task catalogue), `--e2e` (offline run against a mock),
  turn metrics in `~/.rijo/data/turn_metrics.jsonl`.
- **English throughout**: runtime interface, source comments and documentation
  (the TUI strings were already English before the public release; the remaining
  comments and messages followed).
- **Answer language follows the input**: the system prompt states the rule —
  a German message gets a German answer, an English message an English answer,
  anything without a clear language defaults to English. Stored memory entries,
  skills, tool output and older turns are explicitly declared to be content, not
  an instruction to switch language (before that rule, German memory entries
  made Rijo Code answer English requests in German). The learning review writes
  skills and memory entries in English.

### Brain

- The workspace is indexed into `~/.rijo/data/brain.db` (chunks per language
  family, embedded with fastembed `bge-small-en-v1.5`) and searched every turn;
  top hits reach the system prompt as file:line pointers.
- `brain_search` lets the model query the index itself: pointers, scores and
  bounded excerpts, scoped to the current workspace, with a score floor
  (default 0.30). If the embedder is busy it falls back to full text and says so.
- `rijo --index` fills the index explicitly; one-shot runs skip background
  indexing.
- Hybrid retrieval: vector and FTS5 both run and are fused by reciprocal rank
  (RRF) — paraphrases from the embedding, exact identifiers from the keyword
  path; a hit both paths like ranks first. Reported as `hybrid`/`vector`/`fulltext`.
- Incremental indexing via an `indexed_files` table (mtime + size + chunks):
  a warm start only stats the files, a changed file replaces its own chunks
  (no ghosts), deleted files lose theirs. Replaced a per-start re-embedding pass.
- `brain_search` hits bump `access_cnt`, used as a ranking tie-break.
- Each turn logs `brain_ms`, `brain_hits`, `brain_top_score` and `brain_mode`,
  so the value can be measured instead of assumed. `memory.enabled: false`
  switches the brain off for A/B runs (`scripts/ab_brain.py` measures it).
- Measured: warm index 16-20 ms vs ~65 s before; the cold index of an 80-file
  repo takes 870.9 s. A/B (3 runs per arm, same task): with the brain 3.3
  iterations and Ø 23k prompt tokens, without it 5.0 and Ø 36k — both arms
  correct, n=3 is a hint, not proof.
- Known gap (unchanged): the `edges` table has no writer — there is no graph
  layer yet, the schema is aspirational.

### Embedding model: one cache, fetched at install time

- The ONNX model and tokenizer were downloaded into the *working directory*
  (fastembed's default `./.fastembed_cache`), so every project folder got its own
  128 MB copy. They now live in `~/.rijo/models/fastembed` (override:
  `RIJO_MODEL_CACHE`), and `install.sh` fetches them once after installing the
  binary (`--skip-models` to opt out, `--dry-run` names the step).
- New `rijo --download-models` for the one-off fetch, with a size report that
  does not double-count the HuggingFace symlinks (was reporting 255 MB for 128 MB).
- The initialization also honours `memory.embedding_model` now — the old code
  always built the default init options and ignored the configured model name.

### Context tool: status, recall, show

- New `context` tool (29th): `status` reports the fill against the detected window
  plus the session breakdown, `recall` searches this session's older messages via
  the existing FTS5 index (measured 1 ms against 2-4 s for an LLM call), `show`
  prints the stored compression blocks.
- `sessions.db` gained a `summaries` table with a watermark (`upto_message_id`),
  which is what makes a later compression cost O(new messages) instead of
  O(everything) and keeps it across restarts. `SessionWriter` writes many messages
  over one connection instead of one connection per message.
- Fixed on the way: a recall for a hyphenated term (`ZIMT-4711`) was an FTS5
  syntax error and returned nothing although the message existed; the expression
  now splits on everything but letters, digits and `_`.

### Context window auto-detection, no diagnostic popup

- The model's context window is detected from its name
  (`crates/rijo-core/src/models.rs`); `llm.context_window` overrides it and `0`
  means auto. An unknown model keeps the gauge honest (size without percentage).
- Diagnostic notes from the turn path go to `data_dir/diagnostics.log` only: the
  popup in the right part of the chat area was removed on request (it had replaced
  the earlier `eprintln!`, which corrupted the frame).
- Plan for the chat-history compression tool committed under
  `.hermes/plans/2026-09-18_105329-chat-history-kompression.md`.

### Two status rows: live activity above, measurements below

- The single status row switched between live state and measurements, so the
  numbers disappeared while a turn ran. Now the activity row (spinner, phase,
  live rate, background processes, hints) sits directly above a permanent data row
  (context fill, TTFT, turn times, throughput, cache, session totals).
- The data row shows the live context ESTIMATE while a turn runs (`ctx ≈12k/128k
  9%`) and says `no measurements yet` before the first turn instead of staying
  blank.
- One space between segments (the padding used to produce a double space before
  every `· value`).

### Status line: diagnostics no longer break the frame

- Turn-path diagnostics (context pruning) used `eprintln!` and appeared inside the
  status row, shifting the layout below it. They now go to
  `data_dir/diagnostics.log` and are shown as a TUI toast; stderr is used only in
  headless runs.
- A running turn shows a live context estimate (`ctx ≈12k/128k 9%`), because the
  first turn had no previous measurement to display. The ≈ marks it as an estimate
  (~4 characters per token); the finished turn shows the provider-reported number.

### Status line: context, first-token latency, throughput

- New indicators above the input box: context fill of the last request against
  `llm.context_window` (colour-coded; no percentage when the window is unknown),
  time to first token, real tokens/second, tool failures, brain hits, session
  totals and a marker for still-running background processes.
- `ttft_ms` and `failures` are measured per turn and written to
  turn_metrics.jsonl as well, so the TUI number and the file agree.
- Narrow terminals drop segments from the end; a shortened value list drops whole
  values (marked `…`) instead of cutting a number in half — a cut "Cache 2…" would
  read like 2 %.

### Web search: two latency traps fixed

- A configured Websurfx was never used: `.env` values live in the sealed store,
  but the search tool only read the process environment, so every normal run
  silently answered from Wikipedia. It now resolves the URL from the environment
  first, then from the sealed store.
- A hanging provider cost its full timeout on every search (measured 16.6-17.6 s
  per call). New: bounded provider timeout (`web.websurfx_timeout_ms`, 10 s) and a
  cooldown (`web.websurfx_cooldown_secs`, 15 min) — the first failure costs the
  timeout once, later searches skip the dead provider (measured 376 ms).
- One pooled HTTP client instead of a client per call with a rotating user agent
  (a fresh client pays DNS+TCP+TLS per request: 448-484 ms vs 322 ms measured).
- `web_extract` fetches up to 5 URLs in parallel, input order preserved.
- The search cache key includes the limit (a limit=5 answer was served for a
  limit=20 request). The Wikipedia endpoint is overridable (`web.wikipedia_api`),
  which also makes the fallback testable without the network.
- The Websurfx health probe uses the JSON search route with `json=true` (any path
  returns HTTP 200 from the static page, so a stuck daemon looked healthy) and no
  longer starts a duplicate instance on an occupied port (`AddrInUse` in the start
  log).

### GitHub tool (native REST client)

- New `github` tool (28 tools total) for repository work: repo view/create/delete,
  branches, commits, pull requests, issues, releases, workflow runs, repository
  search, plus a read-only `api` escape hatch.
- Auth from `GH_TOKEN`/`GITHUB_TOKEN`, a configured env var, or `gh auth token` —
  never from a tool argument and never from config.yaml.
- Read results are cached for a few seconds and any write invalidates the cache;
  read actions batch, writes break the batch (args-aware `is_parallel_safe_call`).
- Approval gate for irreversible or public-facing writes (`repo_delete`,
  `pr_merge`, `release_create`, public `repo_create`).
- Measured: `gh repo view` 631-906 ms vs the tool's later calls at the 390-710 ms
  round-trip floor and zero requests for a cached repeat.

### Sync between machines (core, transport next)

- Portable sync directory (`~/.rijo/sync`) with a manifest, per-workspace brain
  JSONL (nodes **with** embeddings), skills and memory. Transport-agnostic: a
  private git repo or rsync moves it, the merge rules live in the core.
- `rijo --sync-export`, `--sync-import`, `--sync-status`. Merging is a set union
  over content-addressed node ids; skills keep both versions on conflict; memory
  entries are unioned and capped. Import refuses on embedding model/dimension
  mismatch, skips workspaces that are not on the local allowlist (with a count),
  and rebases ids when a workspace path is mapped.
- Never exported: `config.yaml`, `.env`, caches, process logs. Workspaces are an
  explicit allowlist because the index contains source code.
- Fixed on the way: an upsert without an embedding no longer wipes the stored
  vectors (the second sync import used to null every embedding — found by a real
  end-to-end run, not by the unit tests; there is a regression test now).

### Known limitations

- Linux-first (developed on Arch/Hyprland), macOS untested.
- The guard is a heuristic, not isolation.
- The effect of the learning feature on actual work is not proven yet (an A/B
  with 5+5 runs on a pitfall task is still pending).
- `debug.rs` always writes the tool trace to `~/.rijo/data/`, even when
  `data_dir` points elsewhere.

### Build notes

- `[profile.release]` lives in the root manifest: `opt-level = 3`, `lto = true`,
  `codegen-units = 1`, `strip = "debuginfo"`. It used to sit in the member crate
  where cargo ignores it, so LTO never ran. With LTO the binary shrinks from
  52.2 MB to 44.5 MB (measured). The tool latency in `--bench` did not change
  measurably within the run-to-run spread of the test machine (2–37 ms for
  `terminal_echo` under varying load).
- A full release build with LTO takes ~30 minutes from scratch and downloads the
  ONNX runtime once (via the `ort` crate).

