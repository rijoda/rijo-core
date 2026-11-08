> **Historical (moved 2026-10-18).** The German goal/measurement document as of October 2026.
> Kept unchanged as evidence; not maintained, and its links refer to old paths. Current state:
> [`../status.md`](../status.md).

# Path to 100 % — what is missing and how to measure it

Goal (verbatim): *"a fully functional coding harness like claude code or hermes
that does every toolcall at maximum achievable speed for faster development
using ai."*

Without a definition the goal is not reachable, because "fully functional" and
"maximum achievable" have no endpoints. So here is a testable
definition: **100 % = all acceptance criteria below are proven by a real run.**
Not "feels finished".

Status: 07.10.2026 — **339 Tests grün** (289 Core-Unit + 2 CLI-Unit + 11 Provider +
29 Integration + 2 MCP-Client + 4 MCP-Tool + 2 Compress-Contract), 1 ignoriert (`router::session::scores_with_real_bundle`, braucht ein exportiertes
ONNX-Bundle), Working Tree clean. Verifiziert mit `cargo test --workspace` auf
Windows 11 (siehe §AB). Die inline genannten Testzahlen in den datierten
Abschnitten unten sind historische Momentaufnahmen; die jeweils gültige Zahl
steht hier und in `README.md`.

---

## A. Acceptance criteria (this is the definition of 100 %)

| # | Criterion | Measurement | Status |
|---|---|---|---|
| A1 | Every registered tool was really invoked, with an independently verified result | `tool_trace.jsonl` `ok=true` + separate verification of the artifact (file content, WAV header, exit) | **26/26** at that time (see section K); since `brain_search` was added on 15.09. the honest count is 26/27 until that tool is proven in a real run |
| A2 | Interactive TUI works (input, cancel, scroll, mouse, resize) | real terminal run | **open** — needs you, crossterm requires a real TTY |
| A3 | Display covers all tools/features, without raw JSON blobs | render tests against `TestBackend` (8 tests) | **met** |
| A4 | Local tool latency beaten against the reference | `rijo --bench`, median of 7 | **met** (112×–1815×) |
| A5 | End-to-end turn time measured and documented | `turn_metrics.jsonl` (duration, iterations, tokens, cache) | **met** (instrumentation), optimization open |
| A6 | Fixed prompt overhead per turn known and reduced | `prompt_tokens` of a trivial turn, split system/schemas | **measured: 4402 tok**, reduction open |
| A7 | Provider cache hit rate visible | `cache_hit_tokens`/`cache_miss_tokens` | **met** — 26 % → 94 % after the prefix fix (section M) |
| A8 | No regression: tests + smoke green after every change | `cargo test`, `--e2e` with `mock_smoke.json` | **met** (72+14, smoke 19/19) |

---

## B. Verification gaps (A1, A2)

Three tools are registered but never proven by a real run:

- `delete_file` — needs the confirmation answer of the TUI; in the headless path
  the confirmation channel must be injectable (confirm tx), otherwise it blocks.
- `clarify` — waits for a user answer; headless only checkable via a
  preset answer path.
- `delegate_task` — starts a child process; with `RIJO_LLM_MOCK` the
  child run should be deterministic, but that has not been checked.

All three are checkable with a test setup (injection of confirmation/answer,
mock provider in the child), without you having to do anything. **Effort: medium.**
A2 (a real terminal run) is the only item that absolutely requires you.

---

## C. Speed (A6, A7) — the measured situation

Trivial turn ("answer with exactly one word: ok"), real DeepSeek calls:

    prompt_tokens      4402   (constant on every turn)
    cache_hit          1024   → only 23 % hit rate
    cache_miss         3378   ← this part is reprocessed every time
    llm_ms              968–1085
    sys_chars           3545   system prompt
    tools_chars        12041   tool schemas  ← 77 % of the fixed overhead

That makes it clear where the speed lies — and where it does not:

1. **Tool schemas (12.041 characters, 77 %).** The biggest lever, completely in
   our hands: shorten descriptions, drop optional fields, remove duplicated
   explanations (the ones the system prompt already gives).
   Measurement: `prompt_tokens` before/after, plus an A/B on a fixed task, so
   that tool selection does not get worse. **Effort: medium, effect: high.**
2. **Cache hit rate (23 %).** The cache evidently only applies to the
   system prompt part. A stable prefix helps: constant order of
   system → tools → history (never insert anything dynamic before the tools).
   **Effort: small, effect: medium.** → **done (section M):** the cause
   was not only dynamism, but the per-process random tool order from
   `AHashMap`. After sorting: 26 % → 94 % hit rate.
3. **Iterations (median 2).** Every iteration is a full LLM roundtrip.
   Parallel batching already helps; repeated/failing tool calls
   cost whole iterations. A guard against repetitions exists —
   effect not measured. **Effort: small, effect: medium.**
4. **Local tools are no longer an issue:** bench 112×–1815× faster,
   tool time normally 0–6 ms. The turn is dominated by LLM latency and
   prompt size, not by tool execution.

---

## D. Feature gaps compared with Claude Code / Hermes

Deliberately not rated as "must", but as a decision list:

| Capability | Status | Effort | Benefit |
|---|---|---|---|
| MCP client (external tool servers) | **increment 1 done** — `mcp` tool (stdio, proxy action, guard-gated per `read_only_tools`; ADR 0017) | medium (rest: HTTP, named tools, pooling) | ecosystem integration |
| MCP server (expose the native tools to other agents) | **done** (`rijo mcp serve`, 2 meta-tools over 31 tools, Guard-routed) | small (done) | reach — usable from Claude Code, Codex, Cursor, VS Code |
| Hooks (programs bound to events) | missing | small–medium | automation |
| Diagnostics feedback (compiler/lint errors directly into the context) | **done** (`diagnostics.rs`; attached to `write_file`/`patch` results when `diagnostics.after_edit` is on) | small (done) | fewer failed attempts, fewer iterations |
| Checkpoints/undo for file changes | missing | medium | safety net for large changes |
| Cost/token budget per session | counter present, no limit | small | cost control |
| Plan mode (plan first, then execution) | missing | small | fewer wrong turns |
| Interactive confirmation of destructive actions | gate for commands AND code, TUI dialog, headless fail-closed | small (done) | prevents data loss (section N) |
| Start server/dev server in the background and **keep working** | `background: true` + process registry; self-backgrounding (`&`, `nohup`) is redirected | small (done) | check a website live while building continues (section O) |

---

## E. Order (proposal)

1. **Shorten tool schemas** — the biggest measured lever, A6; procedure below in
   section G (measurement setup exists, first results in section H).
2. **Close A1** — prove `delete_file`, `clarify`, `delegate_task` with injected
   answers/mock (I can do that without you).
3. ~~**Stabilize the cache prefix** — small change, measurable via
   `cache_hit_tokens` before/after.~~ → **done on 14.09. (section M)**: 26 % →
   94 % hit rate, −91 % cache miss tokens per turn.
4. **Diagnostics feedback** — lowers iterations where code is compiled.
5. **A2 with you** — one real TUI run; after that the harness is completely
   proven.
6. Optional, your choice: MCP, hooks, checkpoints, budget limit.

---

## G. Measurement setup for step 1 (schema evaluation)

So that shortening is not guesswork: `crates/rijo-core/src/schema_eval.rs` plus
`data/schema_tasks.json`.

Two tools:

    ./rijo --schema-report      # characters per tool, no API call
    ./rijo --schema-eval        # variants against the task catalogue, real model

The task catalog has 33 tasks that cover all 26 tools; a test
enforces this coverage. What is scored is the **first** tool decision
(temperature 0, non-streaming, real system prompt).

Two things the setup deliberately distinguishes:

- **Acceptable tools instead of one expected tool:** often several paths
  lead to the goal (`terminal` vs. `search_files`), and both are correct.
- **Documented intermediate steps:** some tasks necessarily require a
  look ahead — `kill` needs a process id (which only `list` delivers),
  `browser_type` needs an element from the snapshot, writing often needs an
  existence check first. These runs are counted separately
  (`precursor`/`precursor_args` per task), because they occur the same way in
  every variant and would otherwise add noise to the description measurement.

Without this separation the first run looked as if all variants had got the
same 7 tasks "wrong" — in fact it was the scorer, not the
description.

---

## H. Results (full run, 33 tasks × 2 repetitions per variant)

Real DeepSeek calls, temperature 0, 66 runs per variant
(`~/.rijo/data/schema_eval/schema_eval-20260914-153815.json`):

    Variant          chars    saving      tool pick  args       intermediate     median
    full               12041        0%     49/66      45/66         14/66        1380ms
    no_param_desc       8733       27%     50/66      46/66         16/66        1118ms
    compact             7768       35%     51/66      47/66         14/66        1196ms
    minimal             5529       54%     50/66      44/66         14/66        1160ms

Strictly correct (tool AND arguments): full 45/66 (68 %), no_param_desc 46/66
(70 %), compact 47/66 (71 %), minimal 44/66 (67 %).

What follows from that — and what does not:

1. **Shortening measurably costs no quality.** The differences (1–3 runs out of 66)
   are within noise (at p≈0,7 and n=66 the spread is ≈2,7 runs). No
   variant is significantly worse, and 27–54 % fewer schema characters
   save ~200 ms median latency (1380 → 1118–1196 ms, 13–19 %). This
   latency effect is consistent across all three shortened variants and therefore
   the most robust signal.
2. **The prose is not the problem.** `--schema-report`: for `process_manage`
   174 characters of description stand against 752 characters of parameters; for
   `skill_manage` 57 against 595. The five heaviest tools are 37 % of the
   overhead. So shortening has to start with the parameter definitions.
3. **Parameters must not be removed, only shortened.** The `minimal` variant
   (only required parameters) is the only one with visible damage: with
   `terminal`, `background` was missing, and the model fell back to
   `nohup … &` on the command line. A fallback/workaround behaviour
   that breaks the parameterization — therefore: parameters with type and enum
   stay complete, what is shortened is their description text.
4. The best candidate is therefore a **hand-written `compact` version**
   (emoji gone, one sentence per tool, terse parameter texts) — the mechanically
   generated `compact` variant already shows 35 % savings without quality loss;
   by hand more is possible, without trimming the parameters.
5. Honest caveat: 68–71 % absolute hit rate sounds low, because
   the scoring measures the *first* decision. 14–16 runs per variant were
   documented intermediate steps ("look first"). The rate is therefore a
   comparative measure between variants, not an end-customer quality verdict.

---

## I. Implemented: hand-written shortening (2026-09-14)

On the basis of the results, the shortening was written directly into the tool
schemas (not as a transformation stage, but as real text):

- The emoji prefix drops out of the LLM schema (the `--tools` list keeps it).
- Tool descriptions reduced to the essentials, the tool-steering hints
  are retained ("PREFERRED for edits", "Use instead of grep/find/ls",
  "NEVER deletes .env files").
- Parameter texts reduced to what is necessary; **types, enums and required
  fields unchanged** — because the evaluation proved that removing parameters
  harms (`terminal.background` was dropped, the model fell back to `nohup … &`).
- 45 replacements, each checked for exactly one occurrence.

Measured afterwards:

| Metric | before | after | change |
|---|---|---|---|
| Schema characters (sum of the tools) | 9819 | 8057 | −18 % |
| Schema characters (JSON set, as sent) | 12041 | 10279 | −14,6 % |
| Prompt tokens per turn (real run, trivial prompt) | 4402 | 3977 | −425 token (−9,7 %) |
| Tool choice (evaluation) | 49/66 = 74 % | 77/99 = 78 % | not worse |
| Arguments (evaluation) | 45/66 = 68 % | 71/99 = 72 % | not worse |
| Median latency (evaluation) | 1380 ms | 1204 ms | −13 % |

The accuracy differences are within noise (the runs have 66 and 99
repetitions respectively), but no metric has become worse, while every
single turn sends 425 tokens less — permanently, on every call.

What remains open: the same shortening for the parameter **structure** (not only
the texts) has not been investigated; `minimal` showed that this is risky. The
next stage would be merging rare parameters (e.g.
`offset`/`limit` into one expression parameter), which however changes the tool
semantics and therefore needs its own measurement run.

## K. A1 closed — the last three tools (2026-09-14)

`delete_file`, `clarify` and `delegate_task` were registered, but never really
executed. Now proven:

**`delete_file`** (the only destructive tool — here a refusal counts as much
as a success). Two integration tests through the real registry, five cases:
refused without approval and the file stays in place · `.env` refused despite
approval · refused outside the workspace · missing file returns "not found"
instead of a panic · with `RIJO_APPROVE_DANGEROUS=1` and inside the workspace really
deleted. All rules work fail-closed (`None => false` without a TUI channel).

**`clarify`** — returns the questions in structured form (`needs_clarification`,
questions unchanged) instead of swallowing them; a test covers this.

**`delegate_task`** — and here there was a real bug: `run_child` built
`Config::default()` instead of `Config::load(None)`. The subagent thereby ran
against the default provider (OpenAI) without a key. Real run before:

    child failed: LLM API error 401: You didn't provide an API key …

That means: every delegation failed, regardless of the configured
environment — the bug was only invisible because the tool was never executed.
After the fix (load config):

- Child mode directly: `./rijo --child brief result` → the result file contains "4"
- Parent path for real: the model calls `delegate_task` (trace `ok=true`, 3207 ms),
  final answer "The subagent computed 7 × 6 and returned 42 as the result."

A1 is thereby met: 26 of 26 tools proven by a real run. Test status afterwards:
61 unit + 12 integration tests green.

Only A2 remains open (TUI on a real terminal), because crossterm needs a real
TTY.

## L. Diagnostics feedback (2026-09-14, implemented, off by default)

Item 4 of the order: after a change to a source file, collect the
compiler output and append it to the tool result. That way the
error is in the same response instead of one iteration later.

Implementation: `crates/rijo-core/src/diagnostics.rs` + `diagnostics:` in the
config; appended to `write_file` and `patch`. Detects the check command from the
project (Cargo.toml → `cargo check`, tsconfig.json → `tsc`, `.py` →
`py_compile`), a custom `command` with a `{file}` placeholder takes precedence.
Timeout kills the process (7 unit tests, incl. timeout, truncation, off case,
non-source code).

Measured with a real model run (deliberate syntax error, then fixed):

| Case | Duration |
|---|---|
| `patch` with diagnostics, error case | 2.057 ms |
| `patch` with diagnostics, success case (full compile) | 22.319 ms |
| `patch` without diagnostics | ~200 ms |

In the same step the model received

    metrics.rs:250:14: error: this file contains an unclosed delimiter

and fixed the error in the next action (one saved iteration ≈ 1,2 s).

**Assessment:** 2–22 s of cost against ~1,2 s of saved iteration per error. On
this project that is only worthwhile if a lot is reworked (an overlooked
error otherwise costs several iterations) or if a faster command is
configured. Therefore off by default — the lever is there, the
decision stays with the user.

---

## J. What I do not call "finished"

- TUI operation without a real terminal run.
- Tools whose smoke test only yields an `ok` flag, without verification of the artifact.
- Speed as a claim without `turn_metrics.jsonl` numbers.

## M. Prompt cache: cause found and fixed (2026-09-14)

`AHashMap` scatters the iteration order per process — `Registry::get_names`
passed it on exactly like that, and the tool schemas went to the provider in this
random order. Every process start therefore saw a different prefix;
the automatic prompt cache only hit the part before it (the system prompt),
the ~2.900 token of schemas were reprocessed every time. The clue was in
the number itself: the hit stuck exactly to the system prompt (1024 token) and
stayed there, no matter how often the same request ran.

Measurement setup: a local capture proxy forwards the request body unchanged
(`--config` with a redirected `base_url`, `RIJO_SKIP_TRUST=1`). That makes
the bodies comparable byte by byte, and the cache numbers come from the real
provider response instead of an estimate.

    Metric              before            after
    Cache hit           1024 / 3967       3712 / 3967     26 % → 94 %
    Cache miss          2943              255             −91 %
    Body (sha256)       differs per run   identical (3/3)
    Tool order          random            alphabetical

Fix: `names.sort_unstable()` in `Registry::get_names()` — one line. The rule
behind it: everything that goes into the prompt must be byte-identical across
process boundaries; the iteration order of a map is not.

Evidence: regression test `tool_order_is_deterministic_and_sorted` (two
registries must deliver the same order — without sorting it fails in
3 of 3 runs, checked by mutation), schema evaluation after the fix with
tool choice 50/66 and arguments 46/66 (reference: 49/66 and 45/66 respectively,
difference within noise at n=66), 95 % cache hit rate in the eval run itself.

## N. Gate for destructive actions — class fix (2026-09-15)

A real run as the trigger: `rm -rf ./*` was blocked three times in `terminal`
(directly, with an env prefix, with `export`) — and the model then deleted the
workspace via `execute_code` (`shutil.rmtree`) in one step, including `.git`.
The folder was empty afterwards.

Cause in one sentence: the gate hung off the **tool** (`terminal`) and checked
**text**. Every other execution path was therefore unchecked, and every other
spelling too.

Implemented (`crates/rijo-core/src/guard.rs`):

- one gate for commands AND code payloads, used by `terminal` and
  `execute_code`;
- the content of started scripts is checked as well (`bash cleanup.sh`);
- self-approval (`RIJO_APPROVE_DANGEROUS` in the payload) is hard
  rejected, and the error message no longer offers a bypass;
- approval only via the TUI dialog, headless fail-closed;
- the system prompt states the rule (do not bypass, do not repeat in the other tool,
  report and stop);
- on errors the TUI shows the message instead of "ok" — a blocked action was
  labelled ✗ with the text "ok", the reason was invisible;
- audit trail `~/.rijo/data/guard_trace.jsonl`: every decision as a line
  (`denied-headless`, `denied-by-user`, `approved-by-user`, `approved-env`,
  `self-approval-rejected`) with target and reason; redirectable for tests via
  `RIJO_AUDIT_LOG`. The tool trace had omitted the target for `delete_file` and
  `execute_code` (`{}`) — precisely with the destructive tools
  the traceable information was missing; now the path or code is in there
  (truncated to 400 characters).

Proven: 7 unit tests in the guard module plus two integration tests through the real
registry. The first checks five rejection paths (blocked `rm -rf`, blocked
python path, detected self-approval, detected script, harmless counter-check
runs). The second drives the TUI dialog programmatically and proves the
other side: with approval `rm -rf` and `delete_file` really delete, with a refusal
the file stays in place — the gate does not forbid everything, but lets exactly
the user's decision through. The dialog code lives once in `guard.rs` and
is used by `terminal` (destructive commands and sudo), `delete_file` and
`execute_code`.

Tests that set the approval variable now run serialized — otherwise
they grant each other the approval and the rejection cases silently
turn green.

Open, explicitly **not** called solved: a text heuristic is
no sandbox. A different interpreter, base64-encoded or runtime-built
payload get past it. A real guarantee exists only with a sandbox (bubblewrap/
Landlock) or a restricted user — the next substantial step.

## O. Server in the background instead of a hanging turn (2026-09-15)

Real run: the model built a website and wanted to check it live —
`cd … && { python3 -m http.server 8712 >/dev/null 2>&1 & } ; sleep 1; curl …` as a
foreground command. After that the turn stood still, the call is missing from
`tool_trace.jsonl` (it never came back), and bash + http.server kept running
as orphans (port 8712 occupied, the process was still running hours later).

Cause: the command's stdout/stderr are pipes. The shell was done after the
`curl`, but the server held the write end open — `read_to_end()` waits for
EOF, which only comes with the end of the server. The timeout check ran only
as long as the direct child lived; afterwards there was no deadline any more.

Fixed in four steps:

1. Commands that send themselves into the background (`&`, `nohup`,
   `setsid`; not `&&`/`2>&1`/`&>datei`) are redirected into the process
   registry: immediate answer with `session_id`, output into files,
   controllable via `process_manage`, marked as `detected_backgrounding`.
2. `read_bounded` (300 ms budget, non-blocking) instead of `read_to_end` — the
   safety net for every command with inherited pipes.
3. Process groups (`process_group(0)`) and `killpg` on timeout, `kill` and
   `shutdown_all`: no more orphans.
4. System prompt: start servers with `background: true`, check in a SEPARATE
   call, no output to `/dev/null`.

Proven: three integration tests — the original command comes back in <10 s with
`session_id` and can be killed; a foreground call with an inherited pipe comes
back in <5 s; a timeout really removes the child process (check via
`/proc/<pid>`). Mutation: without the redirect the first test fails.
Status: 81 unit + 19 integration tests green.

Open (deliberate): a `background: true` process ends with the session
(`shutdown_all` cuts it off). For a permanent "putting it live" beyond the
session an explicit detach switch is missing — and then the user also has to
know how to end the process later.

## P. Terminal latency (2026-09-15): 50 ms poll sleep removed

`exec_foreground` polled the child in a loop (`try_wait` +
`sleep(50ms)`); the first attempt practically always returned "still running", so
EVERY command cost at least 50 ms. Rebuilt to use a waiting thread +
`recv_timeout(deadline)` plus two reader threads that continuously collect
stdout/stderr (which at the same time fixes the child blocking on a full
pipe buffer of ~64 KB).

    terminal_echo   before 50.974 µs   after 2.549 µs   ~20x
    Speedup vs. Hermes    116x → 2.325x      (bench, median of 7, warmup 2)

The lower bound of ~2 ms is process start cost (bash + echo). Further
micro-optimization brings little: the tool time is 0–6 ms against ~1 s of
LLM time per iteration. The lever is the number of iterations, not the spawn.
Covered: `short_command_returns_without_poll_delay` (<30 ms) and
`large_output_does_not_deadlock_the_child` (200 KB output).


## R. Brain: Analyse und Umbau (2026-09-16)

Ausgangsfrage: Wie funktioniert das Gehirn, was bringt es genau, und laesst es
sich besser nutzen? Analyse aus Code, Datenbank und echten Messwerten:

**Befund vorher**

- Der Index war nicht workspace-gebunden: 8 von 74 Quellen existierten im
  aktuellen Workspace nicht (u.a. `src/calculator.rs`, `index.html` und das am
  selben Tag geloeschte `skills.rs`). Knoten-IDs waren rel-pfadbasiert, also
  kollidierten `src/main.rs` zweier Projekte.
- Keine Score-Schwelle: die Top 3 wurden auch bei Kosinus 0.2 injiziert.
- Es gab KEIN Tool fuer das Modell, obwohl der Indexer-Docstring
  `brain_query` als read_file-Ersatz versprach — der Nutzen lag brach.
- Der Abruf war ein Vollscan ueber alle Embeddings (1,25 MB Blobs, 854
  Kosinus-Rechnungen pro Turn), unter dem DB-Mutex.
- Die Wirkung war nicht instrumentiert: keine Brain-Felder in den Metriken.
- `edges` (Kanten) hatte keinen Schreiber, `access_cnt`/`relevance` wurden nie
  fortgeschrieben.

**Umgesetzt (Schritte 1-5, je mit Tests)**

1. Workspace-Spalte + additive Migration, `SearchQuery`-API mit `kind`,
   `workspace`, `top_k`, `min_score`; Treffer unter der Schwelle werden
   verworfen; `SearchMode` sagt dem Aufrufer, welcher Pfad lief. Konfiguration
   `memory.{enabled,workspace_only,top_k,min_score}`. Metriken: `brain_ms`,
   `brain_hits`, `brain_top_score`, `brain_mode`. Nebenbei behoben: der
   Dedup-Hash war nicht breit genug, Aufrufer schneiden ihn → latente Panik.
2. `brain_search`-Tool (27 Tools): Zeiger, Score und Auszug (500 Zeichen,
   max. 20 Treffer), optional `kind`/`path_prefix`/`all_workspaces`; leeres
   Ergebnis nennt die Alternative (`search_files`) und unterscheidet "nichts
   indexiert" von "nichts ueber der Schwelle". `rijo --index` fuellt den Index
   explizit (Einmal-Laeufe ueberspringen die Hintergrund-Indexierung).
3. Inkrementeller Index ueber die Tabelle `indexed_files` (mtime+size+chunks):
   ein warmer Start liest nichts, geaenderte Dateien ersetzen ihre Chunks,
   verschwundene Dateien verlieren ihre Knoten. Testbarkeit ueber
   `trait ChunkEmbedder` + Stub (kein ONNX-Modell im Test).
4. Hybrid-Retrieval: Vektor und FTS5 laufen beide, Fusion per reciprocal rank
   (k=60). Vorher galt entweder/oder.
5. `access_cnt` wird von `brain_search`-Treffern hochgezaehlt und als Tie-Break
   im Ranking benutzt (kein Score-Boost).

**Gemessen**

- Indexierung: **kalt 870,9 s** (80 Dateien, 988 Chunks, Maschine unter Last),
  **warm 20 ms und 16 ms** (jeweils 80 Dateien, 0 geaendert). Vorher lief bei
  JEDEM Start die Re-Embedding-Schleife; aus echten TUI-Laeufen gemessen
  65.372 ms und 70.357 ms. Die 65-70 s waren also der warme Fall — der wird
  jetzt von 16-20 ms ersetzt, Faktor ~3000.
- Der kalte Lauf bleibt teuer. Ehrlich: fuer dieses Repo ~15 min auf einer
  ausgelasteten Maschine; das ist der Preis fuer den ersten Index.
- `brain_search` im echten Lauf (Schritt 2): das Modell rief es zweimal auf
  (ok, 67/528 ms) und antwortete mit `crates/rijo-core/src/tools/mod.rs` und
  `Registry::build_default` — der Treffer stimmt mit der Datei ueberein.
- Turn-Kosten vorher/nachher: `brain_ms` 1.498 ms im Debug-Build (Query-Embedding
  inklusive).

**A/B: was bringt das Gehirn wirklich?** (`scripts/ab_brain.py`, 3 Laeufe je
Arm, Aufgabe "wo wird die Tool-Registry aufgebaut", beide Arme mit identischer
Config ausser `memory.enabled`, Lern-Review waehrend der Messung aus):

    brain ON   Iterationen Ø 3,3 | Tool-Aufrufe Ø 3,3 | search_files Ø 1,0 | read_file Ø 1,3
               Fehlversuche 0 | prompt-Tokens Ø 23.066 | richtige Antwort 3/3
    brain OFF  Iterationen Ø 5,0 | Tool-Aufrufe Ø 6,0 | search_files Ø 3,3 | read_file Ø 1,7
               Fehlversuche 0 | prompt-Tokens Ø 36.299 | richtige Antwort 3/3

Beide Arme antworten richtig. Mit Gehirn ist der Weg kuerzer und vor allem
gleichmaessiger (3-4 Iterationen in allen drei Laeufen, ohne Gehirn 3, 3 und 9
Iterationen: ein Lauf brauchte 8 `search_files` und 12 Aufrufe, 71.952 statt
18.404 prompt-Tokens). n=3 ist ein Hinweis, kein Beweis — die Streuung ohne
Gehirn ist aber genau das, was der Index abfangen soll.

Ehrlich zur Kontamination: der erste Durchlauf wurde verworfen, weil parallel
`cargo test` lief und dessen Tool-Aufrufe (delete_file/execute_code der
Guard-Tests) im selben Trace-Fenster landeten — der Treiber ordnet ueber
Zeilenzahlen zu. Der zweite Durchlauf lief ohne Nebenaktivitaet.

**Was das Gehirn pro Turn kostet**

`brain_ms` je Lauf (frischer Prozess, also inklusive ONNX-Modell-Ladezeit):
2.442-3.337 ms in den sauberen Laeufen, 6.782 ms in einem Lauf, waehrend die
Maschine durch den Testlauf belastet war. Der Vektor-Scan selbst ist billig
(1,25 MB Blobs, ~1.000 Kosinus-Rechnungen; Python-Referenzmessung 73-75 ms,
Rust deutlich weniger) — die Zeit geht in Modell-Init und Query-Embedding. Im
laufenden TUI-Prozess faellt die Initialisierung nur einmal an, exakt gemessen
ist das noch nicht (braucht einen Mehr-Turn-Lauf).

**Bewusst offen (nicht gebaut)**

- `edges`/Graph-Ebene: Schema ohne Schreiber. Entweder mit Relationen fuellen
  (Aufruf-/Referenzkanten per Regex, geringe Genauigkeit) oder entfernen.
- `relevance` wird weiter nicht fortgeschrieben.
- Multilinguales Embedding-Modell: `bge-small-en-v1.5` ist englisch; deutsche
  Inhalte/Anfragen treffen schlechter. Wechsel waere ein eigener Messlauf
  (Modell-Download, Dimensionen, RAM).

## Q. Auto-skill learning — gap closed (2026-09-15)

The old state was not a learning feature but a usage statistic:
`skills.rs` counted tool calls and wrote template files from them to
`<skills_dir>/_auto/tool-<name>/SKILL.md` ("The agent used `terminal`
repeatedly"). No knowledge, invisible (one level below the root, so never
listed by `skills_list`) and without effect (`use_count` was read nowhere).
The 11 placeholders are deleted, `skills.rs` is removed.

Now (Hermes parity, `crates/rijo-core/src/learning.rs`):

- **Trigger:** end of a turn, but only if `iterations >= learning.min_iterations`
  (default 2) and at least one tool call took place. Trivial turns cost
  nothing.
- **Flow:** a dedicated LLM client (temperature 0, `learning.max_tokens` = 800)
  receives the turn excerpt — user request, tool calls with OK/FAIL and
  duration, short form of the args, answer excerpt, existing skill names.
- **Format:** the answer is forced into a machine-readable format
  (`SKILL_CREATE:` + `description:` + body, `SKILL_UPDATE:`, `MEMORY:`,
  or `NOTHING`). The parser discards unknown things, caps at
  `max_actions` (3) and accepts only harmless names (`a-z0-9-_`, no `..`,
  no `/`).
- **Writing:** exclusively to `skills_dir/<name>/SKILL.md` (with
  provenance note `source: turn-review`) and `<data_dir>/memories/MEMORY.md`.
  Skills in foreign libraries (e.g. Hermes) are NOT touched, only
  noted. If a skill already exists, the lesson is appended instead of
  overwritten. No deletion.
- **Visible:** unlike `_auto`, the new skill lies at the top level
  and therefore appears in `skills_list`.
- **Cost:** its own entry `kind: "learn"` in `turn_metrics.jsonl` with
  prompt/completion tokens and duration; can be switched off via
  `learning.after_turn: false`.
- **No loss:** the review runs in the background (`tokio::spawn`), so the
  user does not wait. A counter of running reviews plus
  `wait_for_learning(budget)` in the one-shot path (`--query`) prevent the
  process from aborting the lesson before the end.

First real run (release debug, real model, task "start an HTTP server,
check with curl, stop it again"): It created
`skills/local-http-server-smoketest/SKILL.md` with the concrete steps —
`background: true` instead of `&`, curl check in a SEPARATE call, stop via
`process_manage`, verification via `ps`/HTTP code — including pitfalls.
Cost: 1175 prompt + 674 completion tokens, 3,8 s, in the background.

### A/B measurement (2026-09-15): the skill lay unused

2 runs each with/without a learned skill, identical task (start HTTP server,
curl check, stop; own port, own directory, learning review off during the
measurement, off):

    Arm A (Skill da)   Ø 6,0 Iterationen  Ø 6,0 Tool-Aufrufe  0,0 Fehlversuche  Ø 62,1 s
    Arm B (Skill weg)  Ø 6,5 Iterationen  Ø 6,0 Tool-Aufrufe  0,5 Fehlversuche  Ø 71,1 s

No measurable difference — and the cause is proven: in 4 OF 4 runs
the model called neither `skills_list` nor `skill_view` (trace window per run in
~/.rijo/data/tool_trace.jsonl). The learned skill was present, but invisible
to the model: Rijo Code had no skill index in the system prompt.

Fix: `render_index(&list_skills(state))` appends the skill list (name +
description, max. 40 entries) to the END of the system prompt — Hermes'
progressive disclosure, at the position with the least cache damage.
`skills_list` and the index read the same function, drift is excluded.
Tests: 3 unit (format, truncation, empty list) + integration
`a_skill_on_disk_reaches_the_system_prompt` (disk -> list_skills -> index ->
end of prompt).

Still open: A/B repetition WITH the index (proof that arm A now loads the skill
and thereby works shorter or with fewer errors) as well as the effect of the
index on sys_chars/cache hits in turn_metrics.jsonl.

Known inconsistency on the side: `debug.rs:12` always writes the tool trace to
~/.rijo/data/, even if `data_dir` is set differently in the config — for the
measurement, runs were therefore assigned via time windows.

## S. Sync zwischen Maschinen (2026-09-16)

Frage: Wie laesst sich Rijo Code so syncen, dass ein zweiter Host denselben Stand hat
(Skills, Gedaechtnis, Brain-Index) — moeglichst automatisch ueber ein privates
GitHub-Repo?

**Analyse**

- `~/.rijo` ist 1,2 GB, davon 1,1 GB Websurfx-Checkout, 54 MB Chrome-CDP-Profil,
  51 MB `staging` — reine Caches, nie syncen. Echte Substanz: brain.db 6,6 MB
  (0,86 MB Text + 1,48 MB Embeddings, 1011 Knoten), sessions.db 656 KB,
  skills 3,8 KB, MEMORY.md 1,2 KB, Logs ~630 KB.
- Der Brain-Index ist **inhaltsadressiert** (`code:<ws-tag>:<pfad>:<chunk-hash>`):
  dasselbe Chunk ergibt auf jeder Maschine dieselbe ID, also ist der Merge eine
  Mengenvereinigung statt Konfliktaufloesung. Nur `label` (Zeilennummern) und
  `access_cnt` bewegen sich.
- Embeddings mitzuschicken vermeidet den kalten Index (gemessen 870 s fuer dieses
  Repo). Dafuer muss das Modell auf beiden Seiten gleich sein — sonst ist der
  Import wertlos, deshalb bricht er bei Modell-/Dimensionsabweichung ab.
- Transport: `gh` ist eingeloggt (Scope repo) und `gh auth token` funktioniert →
  HTTPS+Token ohne manuelles Setup; SSH zu GitHub scheitert auf dieser Maschine
  (`Permission denied (publickey)`), Syncthing ist fuer lebende SQLite-Dateien mit
  WAL der falsche Weg (Korruptionsrisiko).

**Umgesetzt: der portable Kern** (`crates/rijo-core/src/sync.rs`)

- `rijo --sync-export` schreibt `manifest.json` (Version, Host, Embedding-Modell,
  Dimension, Workspaces, Zaehler), `brain/<tag>.jsonl` (ein Knoten je Zeile, inkl.
  base64-Embedding), `skills/` und `memories/MEMORY.md`.
- `rijo --sync-import` validiert Version + Modell + Dimension, vereinigt
  Brain-Knoten (Label LWW, Zaehler max), kopiert fehlende Skills, legt bei
  beidseitiger Aenderung `<name>.conflict-<host>.md` daneben, vereinigt
  Gedaechtnis-Eintraege und kappt bei 2200 Zeichen (aelteste fallen weg).
- Workspace-Allowlist: ohne Eintrag wird nichts exportiert; fremde Workspaces
  werden beim Import gezaehlt uebersprungen, ein `workspace_map` verlegt die IDs
  auf den lokalen Pfad.
- Nie im Export: `config.yaml`, `.env`, Caches, Prozess-Logs — per Test abgesichert.

**Gemessen/verifiziert**

- Echter Lauf: Export auf Datenbestand A = 1011 Knoten, 3 Skills, Gedaechtnis,
  3,2 MB Verzeichnis. Import in eine frische DB = 1011 Knoten, beim zweiten Lauf
  0 neue (idempotent). Danach ein echter Modellaufruf auf der importierten DB:
  `brain_search` fand `crates/rijo-core/src/tools/mod.rs` und
  `Registry::build_default` — der importierte Index ist benutzbar.
- Der echte Lauf deckte einen Fehler auf, den die fuenf Unit-Tests nicht sahen:
  der Merge-Zweig schrieb `embedding=NULL` und loeschte damit beim zweiten Import
  alle Vektoren. Fix: `COALESCE(excluded.embedding, nodes.embedding)` plus
  Regressionstest (Mutationsprobe: ohne den Fix faellt der Test).

**Naechster Schritt (nicht gebaut)**

- Git-Transport + `rijo --sync-init`: privates Repo ueber `gh repo create`
  anlegen, Zugangsdaten ueber `gh auth setup-git` verdrahten (Token bleibt im
  gh-Keyring, nicht in der Rijo Code-Config), Config-Block schreiben, ersten Export
  committen und pushen. Danach `on_start: pull` / `on_exit: push` mit Lock,
  Log und TUI-Zeile.

## T. GitHub-Tool (2026-09-16)

Wunsch: der offizielle `gh`-CLI soll fuer Rijo Code ein eigenes Rust-Tool bekommen,
damit Repo-Arbeit auf GitHub schnell geht.

**Analyse**

- Jeder `gh`-Aufruf kostet Prozess-Start (93-96 ms, gemessen mit `--help` ohne
  Netz), Keyring-Lesen (114-192 ms, `gh auth token`) und einen eigenen
  TLS-Handshake — plus Prosa-Ausgabe, die das Modell erst parsen muss.
- Der reine REST-Roundtrip nach api.github.com liegt von dieser Maschine bei
  432-679 ms (Median 597). Diese Untergrenze ist Netz, nicht Code.
- `gh repo view` (warm) 631-906 ms (Median 809) vs. das Tool mit gepoolter
  Verbindung 390-710 ms pro Folgeaufruf. Ersparnis also rund 200 ms pro Aufruf
  (Start + Keyring) — und ein wiederholter Leseaufruf kostet gar keinen Request.

**Umgesetzt** (`crates/rijo-core/src/tools/github_tool.rs`)

- Aktionen: auth, repo_view/list/create/delete, branch_list, commit_list,
  pr_list/view/create/merge, issue_list/create/close, release_list/create,
  run_list, search_repos, api (nur GET).
- `repo` faellt auf das Git-Remote des Workspace zurueck (geparst aus https-,
  ssh- und git@-Form) — `repo_view` braucht im Checkout keine Argumente.
- Lesecache (Default 30 s, `github.cache_ttl_secs`); JEDER Schreibvorgang leert
  ihn, sonst zeigt das Modell nach einem PR eine veraltete Liste.
- Args-abhaengiges Batching: `is_parallel_safe_call` laesst nur bekannte
  Leseaktionen in einen Batch. Im echten Lauf belegt: zwei `repo_view` liefen
  17 ms auseinander im selben Batch. Schreibvorgaenge (und unbekannte Aktionen)
  unterbrechen den Batch, weil Reihenfolge zaehlt.
- Fehler werden uebersetzt: 401 (Token ungueltig), 403 (Scope/Rate-Limit mit
  Restbudget), 404 (privates Repo ohne Zugriff), 422 (Validierung).
- Freigabe nur fuer Unumkehrbares/Oeffentliches: repo_delete, pr_merge,
  release_create und repo_create mit private=false. PR/Issue anlegen laeuft ohne
  Dialog — sonst gewoehnt sich der Nutzer das Wegklicken an.
- Token NIE als Tool-Argument und nie in config.yaml (Test prueft, dass das
  Schema die Woerter token/password/secret/api_key nicht enthaelt).

**Gemessen/verifiziert**

- Echter Lauf gegen die API: `repo_view` + `run_list` fuer rijoda/rijo-code, Antwort
  korrekt (privat, Default-Branch master, letzte zwei CI-Laeufe gruen, der
  drittjuengste fehlgeschlagen).
- 7 Unit-Tests (Remote-Parsing aller Formen, Lese/Schreib-Klassifikation,
  Zusammenfassung, Cache ohne zweiten Request, Cache-Invalidierung nach einem
  Write, Fehlertexte, fehlender Token) + 2 Integrationstests (Batching-Regel,
  Schema ohne Credential-Argumente). Mutation geprueft: ohne die Lese-
  Klassifikation faellt der Batching-Test.

**Offen**

- Kein GraphQL, keine Pagination ueber `Link`-Header (nur `per_page`).
- Der Lesecache ist pro Prozess und nur im Speicher (bewusst: kein persistenter
  Zustand, der veralten kann).

## U. Web-Suche: zwei Latenzfallen (2026-09-16)

Frage: laesst sich die Websuche verschnellern?

**Befund 1 - Websurfx wurde nie benutzt.** Die `.env`-Werte liegen im
versiegelten Speicher (`trust::SEALED_ENV`), damit das Modell Service-URLs und
Secrets nicht lesen kann. `search_websurfx` las aber nur `std::env` — in jedem
normalen Lauf hiess das "WEBSURFX_URL not set" und die Antwort kam still von
Wikipedia, auch mit laufendem Daemon auf Port 8080. Fix: erst Umgebung, dann
versiegelter Speicher (`websurfx_url()`), plus Regressionstest.

**Befund 2 - ein haengender Provider kostete bei JEDER Suche seinen vollen
Timeout.** Der Trace zeigte fuenf Aufrufe mit 16.673-17.607 ms: Websurfx haengt,
Wikipedia antwortet erst nach dem 15-s-Client-Timeout (Messung 2026-09-16). Der
Daemon selbst war dabei verklemmt (159 % CPU, 14 min alt, `/search` antwortete
auch nach 400 s nicht), waehrend `/health` — ein Catch-all, das fuer JEDEN Pfad
HTTP 200 liefert — gesund aussah.

**Umgesetzt**

- Begrenzter Provider-Timeout (`web.websurfx_timeout_ms`, Standard 10 s, bewusst
  ueber dem Upstream-Budget des Daemons) plus Cooldown
  (`web.websurfx_cooldown_secs`, Standard 15 min): der erste Fehler kostet den
  Timeout einmal, danach wird der tote Provider uebersprungen. Der Hinweis reist
  im Ergebnis mit (`"note"`), damit Modell und Trace den Grund kennen.
- Ein gepoolter HTTP-Client statt Client-pro-Aufruf mit rotierendem User-Agent
  (frischer Client = DNS+TCP+TLS pro Request: 448-484 ms gegen 322 ms gemessen).
- `web_extract` holt seine bis zu 5 URLs parallel (Eingabereihenfolge bleibt).
- Cache-Schluessel enthaelt das Limit (vorher wurde eine limit=5-Antwort fuer
  limit=20 ausgeliefert); Wikipedia-Endpunkt konfigurierbar (`web.wikipedia_api`),
  damit der Fallback ohne Netz testbar ist.
- Health-Sonde nutzt die JSON-Suchroute mit `json=true` und startet bei belegtem
  Port keine zweite Instanz mehr (im Start-Log stand dafuer `AddrInUse`).
- Empfehlung fuer den Websurfx-Checkout: `request_timeout = 4` in `config.lua`
  (war 30) — begrenzt eine tote Upstream-Engine an der Quelle.

**Gemessen**

| Fall | vorher | nachher |
- Suche, Daemon haengt, 1. Aufruf: 16.673-17.607 ms -> 8.750 ms (Deckel greift)
- Suche, Daemon haengt, Folgeaufrufe: 16.673-17.607 ms -> 376-428 ms (Cooldown)
- Suche, Daemon gesund: gar nicht benutzt (still Wikipedia) -> 1.138 ms, Provider
  websurfx, echte Web-Treffer
- Cache-Treffer: 0 ms | web_extract 3 URLs parallel 1.389 ms (curl sequenziell
  1.499 ms fuer dieselben Seiten) | Websurfx direkt 1,3-2,1 s

**Tests**: 7 Stueck (haengender Provider wird nach dem ersten Timeout
uebersprungen; Timeout ist begrenzt; Cooldown 0 versucht es erneut; gesunder
Provider wird genutzt; Cache trennt Limits; Extraktion ist parallel; versiegelter
Pfad wird gelesen). Mutationsproben: ohne die Cooldown-Pruefung faellt der Skip-
Test, mit sequenzieller Extraktion faellt der Parallel-Test (929 ms statt ~330 ms).

**Offen**

- Websurfx-Qualitaet: die Treffer sind teils unscharf (nur DuckDuckGo + Wikipedia
  als Engines). Das ist Provider-Konfiguration, nicht Rijo Code.
- Kein Test, der die echte Websurfx-Instanz anspricht (bewusst: Tests laufen
  offline gegen Stubs).

## V. Statuszeile ueber dem Eingabefeld (2026-09-16)

Frage: welche Anzeigen sind sinnvoll, und wie stellt man sie am besten dar?

**Auswahlkriterien**

- Eine Zeile ist knapp: jedes Segment muss eine Entscheidung stuetzen. Was fast
  immer 0 ist (Fehler, Brain-Treffer), kostet trotzdem Platz — also nur zeigen,
  wenn es ungleich 0 ist.
- Keine erfundenen Zahlen: fehlt die Bezugsgroesse (Kontextfenster), zeigt die
  Zeile nur den Wert ohne Prozent. Eine LIVE-Tokenrate gibt es nicht — der
  Provider meldet seine Token erst am Ende des Streams, deshalb steht beim
  Stromen ein "≈" vor der aus Zeichen (~4 Zeichen/Token) geschaetzten Rate.

**Aufgenommen**

- `ctx 44k/128k 35%` — Prompt-Token des letzten Requests gegen das Kontextfenster
  (`llm.context_window`, 0 = unbekannt). Farbe: dim < 60 %, amber 60-84 %,
  rot+bold ab 85 % (dort greifen Pruning/Kompaktierung). Das ist die Zahl, die
  entscheidet, ob der naechste Turn noch passt.
- `ttft 620ms` — Zeit bis zum ersten GESTREAMTEN Token. Genau die Latenz, die man
  merkt: ein langsames erstes Token fuehlt sich wie ein Haenger an, waehrend die
  Rate danach gut aussieht. Amber ab 5 s.
- `Turn 1.4s · LLM 1.1s · Tools 200ms` — Harness-vs-Modell-Aufteilung des Turns.
- `3× · 5 Tool · →33 tok` — Iterationen, Tool-Aufrufe, Completion-Token; die
  Prompt-Seite steht nicht doppelt drin, das ist die Kontextanzeige.
- `30 tok/s` — echte Rate (Completion-Token / gemessene LLM-Zeit, Turn-Durchschnitt).
- `Cache 80%` — Anteil aus dem Prefix-Cache des Providers.
- `2 failed`, `brain 4` — nur wenn ungleich 0.
- `sess 3 turn(s) · 40k tok` — Sitzungssummen; ein einzelner Turn verschweigt,
  was eine lange Sitzung kostet.
- `bg 2` — laufende Hintergrundprozesse (amber). Ein vergessener Server faellt
  sonst erst auf, wenn der Port belegt ist.

**Darstellung**

- Eine Zeile, zwei Zustaende (laufend / ruhend), Reihenfolge = Prioritaet.
- Bei schmalem Terminal fallen Segmente vom ENDE weg (bereits vorhandene Logik).
  Neu: wird eine Wertliste gekuerzt, fallen GANZE Werte weg und `…` markiert das
  — vorher stand dort "Cache 2…", was wie 2 % liest.
- Golden-Test bei 80 Spalten (`status_line_prioritises_on_a_narrow_terminal`) plus
  Farb-Schwellen und der Fall "Fenster unbekannt".

**Gemessen/verifiziert**

- Echter Lauf: `ttft_ms=861, failures=0` im turn_metrics.jsonl; die neue Zeile
  141 Unit-Tests gruen; `docs/tui-look.txt` mit dem neuen Stand neu erzeugt.

**Offen**

- Die echte Kontextgroesse des Modells steht in der Config (Standard 0 =
  unbekannt). Auto-Erkennung gibt es nicht; die offizielle DeepSeek-Seite liess
  sich hier nicht zuverlaessig auslesen (Tabelle ohne Kontext-Zeile im Extrakt),
  deshalb ist der Wert bewusst nicht geraten.
- Kein Verlauf/Sparkline (z. B. Kontextfuellung ueber die Turns) — dafuer ist die
  Zeile zu schmal.

### Nachtrag V.2 - zwei Fehler aus einem echten Screenshot (2026-09-17)

- `eprintln!` im Turn-Pfad (Kontext-Pruning) landete MITTEN in der Statuszeile
  ("... Ctrl+O=details  d, 0 message(s) dropped (35 in the request)") und
  verschob darunter das Layout: ratatui zeichnet differenziell, ein Schreibzugriff
  am Buffer vorbei bringt ihn aus dem Tritt. Fix: `debug::note()` schreibt immer
  nach `data_dir/diagnostics.log`, zeigt im TUI einen Toast und nutzt stderr nur
  im headless-Lauf. Regressionstest: Event + Logdatei.
- Im ERSTEN Turn hatte die Kontextanzeige nichts zu zeigen (kein Vorwert). Der
  Executor meldet jetzt pro Iteration die geschaetzte Request-Groesse
  (`ProgressEvent::ContextEstimate`), die laufende Zeile zeigt
  `ctx ~12k/128k 9%` mit "~" als Schaetzungszeichen (~4 Zeichen/Token); die echte
  Zahl des Providers ersetzt sie nach dem Turn.

### Nachtrag V.3 - zwei feste Zeilen statt einer wechselnden (2026-09-17)

- Rueckmeldung: die Live-Anzeige (writing etc.) und die Messwerte sollen
  gleichzeitig sichtbar sein. Vorher wechselte EINE Zeile zwischen beiden
  Zustaenden — waehrend eines Turns verschwanden die Zahlen.
- Aufteilung: Zeile 1 (direkt ueber der Datenzeile) = Aktivitaet (Spinner, Phase,
  Live-Rate, Hintergrundprozesse, Hinweise), Zeile 2 = Messwerte (Kontext, TTFT,
  Turn-Zeiten, Durchsatz, Cache, Sitzungssummen). Beide sind dauerhaft da; bei
  einem Turn aendert sich nur die obere.
- Damit die Datenzeile nie leer wirkt: vor dem ersten Turn steht
  "no measurements yet", waehrend eines Turns die Live-Schaetzung
  (`ctx ~12k/128k 9%`) — moeglich, weil das App-Feld `context_window` jetzt beim
  Start aus der Config gesetzt wird (`Executor::context_window()`), nicht erst aus
  dem letzten Turn.
- Beim Umbau aufgefallen und behoben: der Segment-Emitter setzte vor jedem
  "· Wert"-Segment ein zusaetzliches Leerzeichen (doppelte Leerzeichen in der
  Zeile). Segmenttexte tragen jetzt kein Eigen-Padding mehr; die Trennung macht
  nur der Emitter.
- Tests: `both_rows_stay_visible_during_a_turn` (obere Zeile ohne Messwerte,
  untere mit, gleichzeitig), `data_row_explains_itself_before_the_first_turn`.
  145 Unit + 26 Integration gruen; `docs/tui-look.txt` neu erzeugt.

## W. Kontext-Kompression, Schritt 1-4 (2026-09-18)

Plan: `.hermes/plans/2026-09-18_105329-chat-history-kompression.md`. Umgesetzt sind
die Grundlagen und die billigste Schicht (DB-Retrieval).

- **Speicher:** `sessions.db` hat jetzt `summaries(session_id, upto_message_id,
  summary, summary_tokens, created_at)` + Index. Die Wasserstandsmarke macht eine
  spaetere Kompression zu O(neue Nachrichten) statt O(ganze History) und
  ueberlebt Neustarts. API: `save_summary`, `summary_blocks`, `latest_summary`,
  `newest_message_id`, `messages_after`.
- **Writer:** `SessionWriter` schreibt viele Nachrichten ueber EINE Verbindung
  (`record_message` oeffnete pro Nachricht eine) und liefert die eingefuegten IDs
  zurueck — die Kompression braucht sie als Marke.
- **Tool `context`** (29. Tool): `status` (Fuellstand gegen das erkannte Fenster,
  Aufschluesselung der Sitzung, was eine Kompression sparen wuerde), `recall`
  (FTS5 ueber die eigene Sitzung), `show` (gespeicherte Bloecke). Leseaktionen
  sind batch-sicher; `summarize` (LLM + Write) kommt spaeter und darf das nicht.
- **Gemessen:** echter Lauf — `status` 1 ms mit echten Zahlen ("11k / 1.0M ≈ 1 %,
  provider-counted"), `recall` 1 ms. Der Prompt-Wert kommt aus
  `turn_metrics.jsonl` (Provider-Wahrheit), nicht aus einer Schaetzung.

**Echter Fund beim Ausprobieren:** Ein Recall fuer einen Begriff mit Bindestrich
(`ZIMT-4711`) lieferte NICHTS, obwohl die Nachricht in der DB stand — FTS5 liest
den Bindestrich als Spaltenfilter (`no such column: 4711`), die Anfrage war ein
Syntaxfehler. Jetzt wird streng alphanumerisch zerlegt (`ZIMT* 4711*`) und der
Fall ist als Test festgehalten. Nur weil der Recall gegen die echte sessions.db
lief, ist das aufgefallen — der Unit-Test mit einfachen Woertern war gruen.

**Entschieden** (ohne Rueckfrage, im Plan festgehalten): Automatik bei 80 % des
Fensters mit Mindestausbeute 2000 Token und mindestens 5 Turns Abstand (sonst
wird der Prompt-Cache dauernd invalidiert), Bloecke max. 1200 Token, die letzten
12 Nachrichten bleiben woertlich, FTS5 zuerst und Embeddings erst wenn die
lexikalische Suche in echten Sitzungen nachweislich danebenliegt.

**Naechste Schritte:** Task 5/6 (LLM-Zusammenfassung mit Wasserstandsmarke,
inkrementell), Task 7 (Trigger im Executor + Metriken), Task 8 (Segment in der
Datenzeile), Task 9 (echter Langlauf + A/B).

## X. Embedding-Modell: einmal beim Installieren (2026-09-18)

- **Ursache:** `TextInitOptions::default()` laesst fastembed in `./.fastembed_cache`
  cachen — relativ zum ARBEITSVERZEICHNIS. Rijo Code startet aber je nachdem, in
  welchem Projekt es laeuft, also lag in jedem Ordner eine eigene Kopie:
  `~/Projects/rijo`, `~/Projects/test`, `test2`, `test3`, `openram`, ... (je
  128 MB). Das ist die Antwort auf "warum wird bei jedem Start neu geladen".
- **Fix:** ein Cache fuer alle Maschinen/Projekte unter `~/.rijo/models/fastembed`
  (Override `RIJO_MODEL_CACHE`; `HF_HOME` hat bei fastembed Vorrang, was
  dokumentiert gehoert). `EmbedConfig` traegt jetzt `cache_dir` UND `model_id` —
  vorher wurden beide ignoriert (immer Default-Init-Options).
- **Install-Zeitpunkt:** `rijo --download-models` holt Modell + Tokenizer einmal
  (gemessen 36 s, 127,6 MB) und macht einen echten Warmup-Embed, damit nicht nur
  die Dateien da sind, sondern ONNX + Tokenizer wirklich funktionieren.
  `install.sh` ruft das nach der Installation auf; `--skip-models` ueberspringt,
  ein Fehler ist nicht fatal (Retry beim ersten Turn), `--dry-run` nennt es.
- **Groessenanzeige:** `symlink_metadata` statt `metadata` — die
  HuggingFace-Struktur verlinkt jede Datei aus `snapshots/` auf `blobs/`, das
  Mitzaehlen der Links verdoppelte (255 MB statt 128 MB).
- **Tests:** Cache unabhaengig vom CWD, Override greift, Init-Options tragen
  Modell + Cache-Dir, Symlinks einmal, und ein Test embeddet echten Text aus
  einem fremden Arbeitsverzeichnis: kein lokaler Cache, zweites Laden laedt
  nichts nach.
- **Offen:** der Download ist weiterhin ~130 MB bei Erstinstallation. Wer es
  vollstaendig offline braucht, setzt `RIJO_MODEL_CACHE` auf ein bereits
  gefuelltes Verzeichnis und startet mit `--skip-models`; ein Bundling des
  Modells ins Release-Asset waere die naechste Stufe (Release groesste Asset).

## Y. Router (DistilBERT) — M0 bis M4 (2026-09-25)

Der Fix fuer die groesste feste Promptlast: ein lokaler Multilingual-DistilBERT
scored den User-Prompt und der Executor schickt nur die Tools oberhalb der
Schwelle. Vollstaendige Toolliste bleibt immer der Fallback.

- **M0 — Beobachtung, keine Verhaltensaenderung.** `RouterConfig`
  (`enabled:false` per Default), `router_model_dir()`,
  `labels_from_registry()` und `--dump-labels`. Pro Turn schreibt
  `decision_trace.jsonl` mit: Prompt (gekuerzt), gesendete Tools, vom LLM
  gewaehlte Tools, Zeichen-Voll vs. -Gesendet, `fallback`, `router_ms`.
  Das ist die Lerndatenbasis fuer den Retrain.
- **M1 — Daten.** `scripts/router/export_dataset.py` baut 5 775 Zeilen aus
  `schema_tasks.json` (33 Zeilen, **streng eval-only**), 77 Seeds und
  synthetischen Familien ueber alle 29 Tools. Split nach Familie, nicht nach
  Zeile; Leakage-Check gegen den Holdout laeuft mit und PASS. Klassen:
  kleinste 59 Zeilen (`browser_click`), keine Null-Klassen.
- **M2 — Training.** Lokal, CPU, `uv`-Venv auf Python 3.12, `torch 2.5.1+cpu`,
  `transformers 4.46.3`. Loss = ASL(Tools) + 0.3 CE(Toolset) + 0.4 CE(Compact),
  `max_len` 256, 5 der 6 Encoder-Layer eingefroren (~35 % schneller auf CPU,
  Val-mAP8 0.741 statt 0.755). Drei Seeds 42/43/44.
- **M3/M4 — Laufzeit.** `RouterSession` (Tokenizer + ONNX via `ort`) in
  `AppState.router`; `Executor::route_tools` filtert die Toolliste und
  **behaelt die alphabetische Reihenfolge** (Prompt-Cache bleibt bytegleich).
  Ein ONNX-Graph mit allen vier Heads (29/8/5/3), opset 17, plus
  `tokenizer.json` + Label-Dateien + `thresholds.json` + `config.json` nach
  `~/.rijo/models/router/v1/`.
- **Gefundener Fehler (TUI-Test):** `every_palette_command_has_a_handler`
  fuehrt `/update` aus — das ersetzt per `self_replace` die **laufende
  Testbinary** durch `~/.rijo/staging/rijo.new`. Danach war der Test-
  Executable die echte App (52 MB statt 355 MB) und `cargo test` fuehrte
  Trust-Prompt/TUI statt Tests aus. `/update` ist im Test jetzt uebersprungen.
- **Guard-Head: Datenluecke, bleibt aus.** `guard_trace.jsonl` hat 388 Zeilen,
  aber nur **32 verschiedene Texte** — `approved-by-user` 2, `denied-by-user` 1,
  `self-approval-rejected` 1. Der Family-Split gab ganzen Klassen eine Seite
  (val 44/52 `approved-by-user`), der Head|score 0,019 accuracy, also unter
  Zufall. Nach Dedup bleiben 23 Train- / 9 Val-Zeilen: fuer Gate E0b
  (Guard >= Baseline) fehlen die Daten. `heads.guard` bleibt `false`, bis
  echte, vielfaeltige Guard-Trace gesammelt sind (Schritt E4).

### Nachtrag Y.1 — Retrain fuer 31 Tools (2026-10-07)

- **Datensatz:** `mcp`/`arxiv` fehlten (`mcp` 0 Zeilen). Seeds + Synthetik-Vorlagen
  ergaenzt, neu exportiert: **5827 Zeilen, 31 Tools, keine Null-Klasse, Leakage PASS**.
- **Gefundener Bug:** `train.py` rief **nie `opt.zero_grad()`** auf — die Gradienten
  summierten sich ueber alle Batches, die Loss divergierte im ersten Epoch
  (1.5 -> 15 -> 22, mAP8 brach ein). Beweis: derselbe Lauf mit dem Fix faellt
  monoton (0.36 -> 0.023). Behoben.
- **Training:** Vast.ai **RTX 3090 (24 GB)**, full fine-tune (`--freeze-layers 0`),
  6 Epochen, 3 Seeds → **tools mAP8 0.742 / 0.703 / 0.731** (Ø ~0.73, ~Referenz 0.741).
- **Bundle:** ONNX (539 MB fp32) + Tokenizer + Labels (31) + Thresholds in
  `~/.rijo/models/router/v1/`. Der bisher ignorierte Test `scores_with_real_bundle`
  laeuft jetzt **gruen**: Scoring **30 ms Median** (160 ms kalt = Modell-Load), fuer
  „erste 50 Zeilen von Cargo.toml" -> **`read_file` (0.986)**.
- **Windows-Bug im Toolchain:** `read_text()`/`write_text()` ohne `encoding="utf-8"`
  brach unter cp1252 — gefixt.
- **Offen:** Gate E2 (`--schema-eval --router-eval` gegen ohne) mit echtem Modell —
  die eigentliche Entscheidung, ob Routing Tokens/Latenz senkt, ohne die Tool-Wahl
  zu verschlechtern. Der Router bleibt Default **aus** (`router.enabled: false`).

### Nachtrag Y.2 — groesseres Basismodell (2026-10-07)

Kapazitaets-Hebel statt Hyperparameter: **XLM-R base** (`xlm-roberta-base`, 278M,
100 Sprachen) statt DistilBERT-multilingual (66M). `Router` und `export_onnx` lesen
die Hidden-Groesse jetzt ueber `dim` **oder** `hidden_size` (DistilBERT vs.
XLM-R/RoBERTa/BERT).

Gemessen — **gleicher Datensatz**, 6 Epochen, 3 Seeds, full fine-tune, RTX 3090:

| Basismodell | tools mAP8 (3 Seeds) | Scoring-Median |
|---|---|---|
| distilbert-base-multilingual-cased | 0.742 / 0.703 / 0.731 (Ø 0.725) | 30 ms |
| **xlm-roberta-base** | **0.856 / 0.850 / 0.810 (Ø 0.839)** | **20 ms** |
| microsoft/mdeberta-v3-base | 0.663 / 0.754 / 0.665 (Ø 0.694) | — |

→ XLM-R base ist der beste Basismodell-Kandidat; mDeBERTa-v3-base war trotz
gleicher Groesse **schlechter** (Ø 0.694) — vermutlich Tokenizer/Preprocessing,
nicht weiterverfolgt.

**Besser trainieren (v8):** Gegen das fruehe Overfitting (v6 gipfelte in Epoche 2)
Regularisierung eingebaut — `--dropout 0.3 --label-smoothing 0.1 --wd 0.05`,
8 Epochen. Ergebnis auf **demselben Datensatz/Basismodell**:

| | v6 (Basis, 6 Ep.) | v8 (regulaer, 8 Ep.) | **v9 (regulaer, 12 Ep.)** |
|---|---|---|---|
| tools mAP8 (3 Seeds) | 0.856 / 0.850 / 0.810 | 0.855 / 0.920 / 0.794 | 0.892 / **0.920** / 0.878 |
| Ø | 0.839 | 0.856 | **0.897** |
| bestes Seed | 0.856 | 0.920 | **0.920** |

Mehr Epochen mit Regularisierung heben v.a. den **Boden** (0.794 -> 0.878) und
senken die Streuung stark; der Bestwert bleibt 0.920.

**Gate E2 (2026-10-08, 33 Tasks x 3 = 99 Laeufe, DeepSeek):** Baseline (volle
Schemas) gegen Router-gefiltert.

| | Baseline | **mit Router** |
|---|---|---|
| gesendete Schema-Zeichen / Lauf | 17 321 | **1 076 (−94 %)** |
| Tool-Wahl | 78/99 (79 %) | **89/99 (90 %)** |
| Argumente | 70/99 (71 %) | **81/99 (82 %)** |
| Fallbacks | — | **0/99** |
| Median (Router-p95) | 1304 ms | **1230 ms** (195 ms) |
| Cache-Treffer | 97 % | 91 % |

Der Router **spart 94 % der Schema-Zeichen und verbessert die Tool-Wahl** bei netto
neutraler Latenz (die ~0,3 s Scoring werden durch den kleineren Prompt aufgewogen).
Der Cache-Treffer sinkt leicht (96 -> 91 %), weil die gesendete Tool-Menge pro Task
wechselt — die absolute Ersparnis dominiert. `router.enabled: true` ist gesetzt;
voller Lauf ueber alle 33 Tasks erledigt (99 Laeufe, 0 Fallbacks).

**Real-Turn-Token-A/B (2026-10-08):** eine Aufgabe (`--query`, je 1 Turn), einmal
`router.enabled: false`, einmal `true`; `prompt_tokens` aus `turn_metrics.jsonl`:

| | prompt_tokens |
|---|---|
| Router aus | 14 371 |
| **Router an** | **5 248 (−9 123 Tokens, −64 %)** |

Der Router spart in einem realen Turn **~9k Prompt-Tokens** (die volle
Schema-Menge entfaellt). Ehrlich: das ist der **erste** Turn (kalter Prefix); in
spaeteren Turns sind die Schemas Cache-Hits, der reale Vorteil ist dann kleiner —
der Gewinn liegt beim ersten Turn und bei jeder Cache-Invalidierung.

**Mehr-Turn-Sitzung (2026-10-08, 6 Turns, EIN Prozess):** `rijo --repl` (neu)
faehrt Prompts headless in einem Prozess, damit der Provider-Prompt-Cache ueber
die Turns greift (nicht 6 kalte Prozesse).

| Turn | prompt_tokens aus | prompt_tokens an |
|---|---|---|
| 1 | 23 537 | 10 093 |
| 2 | 14 667 | 13 300 |
| 3 | 23 329 | 7 231 |
| 4 | 15 473 | 6 588 |
| 5 | 7 775 | 3 638 |
| 6 | 26 956 | 14 887 |
| **Summe** | **111 737** | **55 737 (−50 %)** |

Der Router **halbiert das Prompt-Volumen ueber die Sitzung** (−50 %). Ehrlich:
die `llm_ms` sind aehnlich (~22 s aus vs ~24 s an) — der grosse Prefix ist beim
Cache-Hit billig, der Nutzen liegt in **Token-Volumen, Kontextfenster und
Tool-Wahl** (E2: +11 Punkte), nicht primaer in der Latenz.

**Veroeffentlicht:** Bundle + Checkpoints + Dataset + Model-Card unter
`huggingface.co/slomo-b/rijo-router` (privat). Lokal:
`~/.rijo/models/router/v1` (Runtime) und `~/rijo-router-backup/` (Sicherung).

Bundle = v8/seed43 (0.920), Scoring-Median **39 ms**, `scores_with_real_bundle`
gruen (`read_file` 0.988). Seed-Varianz bleibt gross (0.794-0.920) — naechster
Hebel: mehr Epochen / zweistufiges Fine-Tune. Regler in `train.py`
(`--dropout/--label-smoothing/--wd`), Defaults unveraendert.

Bundle: `~/.rijo/models/router/v1` (ONNX **1,11 GB** fp32, Tokenizer + SentencePiece).
`router::session::scores_with_real_bundle` gruen: „erste 50 Zeilen von Cargo.toml"
→ `read_file` (**0.995**).

**Getestet und verworfen** (bleiben als Option, Default aus): LR-Scheduler
(warmup+cosine, `--sched`) → Ø 0.685; ein prefix-ausgebauter Datensatz (11,7k Zeilen)
→ Ø 0.685. Beides schlechter als konstanter LR auf dem 5,8k-Datensatz. **Caveat:**
der groessere Datensatz aendert auch die Val-Menge, der Vergleich zu 5,8k ist daher
nicht 1:1 — ein fairer Vergleich braucht `evaluate_router.py` auf `eval_holdout`/
`eval_synth`.

## Z. Windows (2026-09-29/30)

**Ausgangsbefund.** `cargo check --workspace --all-targets` auf Windows 11 brach ab:
`ort-sys@2.0.0-rc.13: no prebuilt binaries available for target x86_64-pc-windows-gnu`.
Die `dist.tsv` des Crates listet acht Targets, darunter `x86_64-pc-windows-msvc`, aber
kein `gnu`. Alles andere kompilierte (1,2 GB `target/`, ein Crate). Die Maschine hatte
keinen MSVC-Linker, kein WSL, kein Docker.

**Loesung: zielabhaengige ONNX-Ladestrategie, kein globaler Schalter.**
Unix behaelt `download-binaries` (Runtime wird beim Bauen geholt, es muss nichts
mitgeliefert werden), Windows nutzt `load-dynamic` (DLL wird zur Laufzeit geladen).
Cargo-Features sind additiv, deshalb stehen die zwei Varianten in getrennten
`[target.*]`-Abschnitten, von denen pro Build nur einer greift; `fastembed` folgt
demselben Split, sonst zoege sein eigenes `ort` wieder `download-binaries` nach.

Ein reines `load-dynamic` haette **Linux gebrochen** — dort war die Runtime bisher
Build-Zeit-Artefakt, als Laufzeitabhaengigkeit haette eine `libonnxruntime.so`
mitkommen muessen. Deshalb die Aufteilung statt einer Umstellung.

**Gemessen:** `ort` 2.0.0-rc.13 verlangt API 24 und lehnt aeltere Runtimes mit
`BadVersion` ab — 1.20.1 wird verworfen, **1.28.0 laedt**. `install.ps1` pinnt diese
Version und begruendet das im Kopf. Das Release-Binary macht danach einen echten
Warmup-Embed (`--download-models`), also ist die Kette ONNX -> Tokenizer -> Modell
bewiesen, nicht nur "Datei vorhanden".

### Fehlerklassen, die nur ein Windows-Lauf zeigt

1. **`rijo_home()` statt `std::env::var("HOME")`** — 23 Stellen mit Fallback
   `/home/user` (`guard.rs` sogar `/tmp`). Windows setzt kein `HOME`, nur
   `USERPROFILE`; der ganze Zustand landete unter `C:\home\user\.blaze` — Tool-Trace,
   Guard-Audit, Trust-Store, Update-Staging, **Provider-Token**, Chrome-Profil,
   TTS-Voices, beide Modell-Caches. `paths.rs` ist jetzt die einzige Quelle.
   Belegt: ein echter `--index`-Lauf schrieb `brain.db` nach `%USERPROFILE%\.rijo`,
   die Kopie in `C:\home\user` behielt ihren alten Zeitstempel.
2. **Der Workspace-Schutz lehnte jedes Schreiben ab** (`trust.rs`). `canonicalize()`
   liefert `\\?\C:\...`, der Workspace wird lexikalisch als `C:\...` gebaut;
   `Path::starts_with` vergleicht Komponenten, also passte `\\?\C:` nie auf `C:` —
   `write_file`/`patch`/`delete_file` antworteten "outside workspace". Unter Linux
   fallen beide Formen zusammen, darum fiel es nie auf. Prefix wird entfernt,
   Vergleich unter Windows case-insensitiv. Allein das loeste **20 der 27**
   Integrationstests. Dieselbe Funktion: `trust_marker()` ersetzte nur `/`, aber `\`
   und `:` sind in einem Windows-Dateinamen ungueltig — der Marker war nicht
   schreibbar.
3. **`execute_code` war auf Windows tot**: hart `python3`. Jetzt einmalige Probe
   `python3` -> `python` -> `py` (nur `--version`, nie der Nutzer-Code).
4. **Der Timeout toetete nichts**: `kill(-pid)` stand hinter `#[cfg(unix)]`, und
   `Child` war schon in den Wartungs-Thread verschoben — es gab nicht einmal ein
   `Child::kill()` als Rueckfall. `process::kill_tree()` versteckt die Plattform:
   `killpg` auf Unix, `taskkill /PID <p> /T /F` auf Windows. Ein Integrationstest
   prueft echten Prozess-Tod.
5. **`which` gibt es nicht**: die Chromium- und die TTS-Erkennung fanden nie etwas,
   auch mit installiertem Programm — ein stiller Totalausfall. `paths::which_bin()`
   liest `PATH` und `PATHEXT` (damit findet `cmd` auch `cmd.exe`), ohne Prozess-Start.
   Auch die `bwrap`-Sonde nutzt ihn jetzt.
6. **Shell-Start:** `shell_command()` gab den Pfad **mit** Anfuehrungszeichen zurueck.
   Fuer `Command::new` sind die Teil des Programmnamens -> `os error 123`
   (ERROR_INVALID_NAME), nicht "nicht gefunden". Ohne Anfuehrungszeichen, und der
   Aufrufer quotet selbst, wenn er den Pfad in einen Shell-String einsetzt.
7. **Hostname** (`sync.rs`): `/proc/sys/kernel/hostname` gibt es nicht, also meldeten
   alle Windows-Hosts `unknown-host` — im Sync-Manifest haetten zwei Maschinen
   denselben Namen getragen. Jetzt `COMPUTERNAME`/`HOSTNAME`, zuletzt ein ehrlicher
   Name mit Workspace-Suffix.
8. **Zwei Gates fragten die falsche Frage.** Der erste echte `install.ps1`-Lauf
   blieb an `--download-models` haengen: das Flag fragte nach Provider und API-Key.
   Danach zeigte sich dieselbe Klasse am Trust-Prompt (`rijo --tools` wartete auf
   einer frischen Maschine). Jetzt gilt: **Onboarding** („braucht das einen
   Provider?") ueberspringt jeder Ein-Schuss ohne Turn, auch `--index`; **Trust**
   („fasst das den Workspace an?") bleibt nur fuer `--index`. Beide Installer
   setzen die zwei Variablen zusaetzlich selbst — ein Installer darf nie
   interaktiv sein.

### Gemessen statt geraten

- **Chromium braucht unter Windows KEINE Zusatz-Flags.** Chrome 154 startet headless
  mit genau den Argumenten des Tools (eigener `--user-data-dir`,
  `--remote-debugging-port`), und `/json/version` antwortet mit HTTP 200. Die im Plan
  vermuteten `--no-sandbox`/`--disable-gpu` sind damit widerlegt und nicht drin.
  Gebraucht wurde aber die **Pfad-Suche**: Chrome und Edge liegen unter
  `C:\Program Files\...` und stehen normalerweise **nicht** im `PATH` — genau darum
  fand die alte `which`-Probe nichts. Der Tool-Fehlertext nennt jetzt auch die
  Windows-Namen und `RIJO_CHROMIUM`.
- **PowerShell 5.1 liest BOM-freies UTF-8 als ANSI.** Ein Skript mit Umlauten oder
  Rahmenzeichen bricht dann im Parser. `install.ps1` ist deshalb reines ASCII, und
  `scripts/rijoda.ps1 new-adr` (im Hauptquartier) wurde gefixt: explizit UTF-8 lesen,
  BOM-frei schreiben.

### Tests

`cargo test --workspace`: **239 gruen** (204 Unit + 8 Provider + 27 Integration),
dreimal hintereinander, auch nach `cargo clean` am neuen Ort. Zwei echte
Test-Defekte fand erst der Windows-Lauf:

- die Integrationstests teilten sich **eine** `brain_test.db`; ein Lock unter Last
  liess `BrainDb::open` scheitern und traf damit einen *anderen* Test im Setup.
  Jeder Zustand bekommt jetzt sein eigenes Datenverzeichnis.
- der Latenz-Test verglich **eine** Messung gegen **eine** Baseline. Jetzt das
  Minimum aus drei Versuchen auf beiden Seiten — der 50-ms-Regress, gegen den er
  schuetzt, waere in jeder Stichprobe.

### Bewusst offen

- **`cmd.exe` ist der Zweitweg, nicht der Hauptweg.** Self-Backgrounding erkennt
  `start` jetzt auch dort (`self_backgrounds_in`), und der Prompt nennt beide
  Schreibweisen — aber bash ist die getestete und bevorzugte Shell. Der `cmd`-Pfad
  ist ausgeführt, nicht gemessen: auf dieser Maschine ist Git Bash vorhanden, ein
  Lauf ohne bash steht aus.
- **Websurfx baute unter Windows nie** (Clone-Bug, Abschnitt AA). Seit ADR 0007 ist
  der Auto-Build aus; wer ihn per `web.autostart_websurfx` einschaltet, startet ihn
  ungehaertet — es gibt kein `bwrap`-Aequivalent (SECURITY.md).
- **TTS** braucht `espeak-ng.exe`/`piper.exe` von Hand.
- **`install.ps1` schreibt den Benutzer-PATH** (kein Admin, kein Systemverzeichnis).
- Der Rename auf **Rijo Code CLI** (Crates `rijo-cli`/`rijo-core`, Binary `rijo`,
  Zustand `~/.rijo`) ist in der Rijoda-ADR `0004` festgehalten; die
  GitHub-Umbenennung `slomo-b/blaze` -> `rijoda/rijo-code` steht noch aus.

## AA. Web-Suche: steckbare Kette statt Websurfx (2026-10-04)

Frage: ist Websurfx der richtige Suchweg, oder gibt es etwas Leichteres und
Schnelleres? Anlass war ein echter Bericht: die Clone-/Build-Anzeige stoerte die
TUI, Websurfx lud nie, und getippter Text landete woanders.

**Befunde**

1. **Websurfx konnte gar nicht entstehen.** `git clone <url> .` verlangt ein
   leeres Zielverzeichnis; der Code legte aber die Lock-Datei hinein (und den
   Fehlmarker danach). Reproduziert: mit nur `.rijo-build-failed` im Ordner bricht
   der Clone mit `exit 128` ab ("destination path '.' already exists and is not an
   empty directory"). `~/.rijo/websurfx` enthielt genau diesen Marker und keinen
   Checkout. Fix: Lock liegt jetzt **neben** dem Checkout, das Verzeichnis wird vor
   dem Clone geleert, gits Fehlergrund landet im Log.
2. **Der Clone/Build schrieb aus dem Hintergrund-Thread direkt ins Terminal**
   (`eprintln!` plus `cargo build` mit geerbtem stderr). Das verschiebt ratatui den
   Puffer — dieselbe Fehlerklasse wie die Diagnostik-Notiz aus Abschnitt V. Alles
   geht jetzt nach `<data>/websurfx-build.log`.
3. **Websurfx braucht laut eigener Anleitung Redis** — unser Code startete nur das
   eine Binary. Zusaetzlich steht Websurfx unter AGPL-3.0, die Repos unter
   MIT OR Apache-2.0.
4. **Der keyless-Metasuchweg ist praktisch tot.** 7 oeffentliche SearXNG-Instanzen
   am 2026-10-04 angesprochen: **0/7 brauchbar** (3x HTML bei HTTP 200, 3x sofort
   429, 1x kein JSON). Google Custom Search JSON ist fuer Neukunden geschlossen,
   die Bing Search API 2025 eingestellt.

**Entscheidung** (ADR `0007`, accepted): steckbare Kette
`Key-API (Brave/Tavily) -> Websurfx/SearXNG (nur wenn URL gesetzt) -> Firecrawl
keyless -> Wikipedia`. Der Key ist derselbe Weg wie bei den LLM-Anbietern; ohne Key
funktioniert die Suche sofort ueber Firecrawl. Der Websurfx-Clone/Build laeuft nicht
mehr automatisch (`web.autostart_websurfx`, Standard `false`); eine laufende
Instanz wird per `WEBSURFX_URL` weiter uebernommen.

**Gemessen** (diese Maschine, 2026-10-04, je 3 Laeufe, Median)

| Anbieter | Median | Key | Ergebnis |
|---|---|---|---|
| Wikipedia | **0,40 s** | nein | immer erreichbar |
| Firecrawl keyless | **1,2 s** warm (erster, kalter Lauf 2,0 s) | nein | `title/url/description` |
| DuckDuckGo HTML | 0,22 s | nein | gescraped; in einem Lauf nicht 200 -> fragil |
| oeffentliches SearXNG (7) | – | nein | 0/7 brauchbar |

Brave/Tavily sind noch **nicht** gemessen — es liegt kein Key vor; die
Literaturwerte (~1,0-1,2 s, eigener Index) sind in der ADR als solche markiert.

**Umgesetzt**

- Kaskade mit begrenztem Timeout pro Anbieter und **Cooldown pro Anbieter**
  (`web.search_timeout_ms`, `web.search_cooldown_secs`; die alten Namen
  `websurfx_*` deserialisieren weiter). Ein toter Anbieter blockiert die anderen
  nicht mehr.
- Endpunkte ueberschreibbar: `web.brave_api`, `web.tavily_api`, `web.firecrawl_api`
  (Tests bleiben ohne Netz).
- Der `note` im Ergebnis nennt jetzt alle zuvor gescheiterten Stufen.

**Tests**: 2 neu (Key-API gewinnt und der Key reist im Header — Capture-Server;
Firecrawl antwortet ohne Key). 9 Such-Tests gesamt, 251 im Workspace gruen.

**Offen**

- Brave/Tavily-Latenz mit echtem Key messen.
- Firecrawl-Limits beobachten (laut Anbieter 1.000/Monat kostenlos) und den
  keyless-Anbieter austauschbar halten.
- DuckDuckGo-HTML als letzte keyless-Stufe pruefen (0,22 s, aber gescraped).

## AB. Re-Verifikation (2026-10-07)

Erster Schritt des priorisierten Fahrplans
(`.hermes/plans/2026-10-07_011950-rijo-code-fahrplan.md`): den dokumentierten
Teststand real nachpruefen, statt ihn zu glauben.

**Gemessen** (`cargo test --workspace --no-fail-fast`, Windows 11, GNU-Toolchain,
Rust 1.98.0; Laufzeit der Suiten ~7 s nach dem Bau):

| Suite | Ergebnis |
|---|---|
| `rijo-core` Unit | 269 passed, 0 failed, **1 ignored** |
| `provider_contracts` (Integration) | 11 passed |
| `tool_contracts` (Integration) | 27 passed |
| Doc-tests | 0 |
| **Summe** | **307 passed, 0 failed, 1 ignored** |

- Deckt sich mit `README.md` („307 tests green"). Die Angabe „251" in §AA war
  eine aeltere Momentaufnahme; §Z („239") war der erste Windows-Lauf.
- Der ignorierte Test ist `router::session::tests::scores_with_real_bundle`
  (`crates/rijo-core/src/router/session.rs:415`), Grund: „needs an exported ONNX
  bundle". Als Datumswert: der Router bleibt ohne Bundle ungetestet im Betrieb —
  konsistent mit `enabled:false`.

**Neuer Befund (Hygiene):** `rijo-core` kompiliert mit **45 Warnungen**
(`cargo test --workspace`), davon 16 automatisch behebbare (unbenutzte Imports,
v. a. `cargo fix`-Kandidaten). Der Rest ist ueberwiegend `dead_code`:
`render_minimap`, `total_wrapped_lines`, `short_reasoning`, `AsyncBridge`,
`jobs_dir`, `DEFAULT_CACHE_TTL`, `reset_all_cooldowns`,
`strip_tags_and_extract_text` sowie `static CANCEL`/`POLL` werden nie benutzt,
dazu zwei `unreachable pattern`, zwei `last_err`- und mehrere „assigned but
never read"-Stellen und eine deprecated `rand::thread_rng`-Nutzung. Das ist kein
Verhaltensfehler, aber es widerspricht dem Anspruch „Tests + Doku sauber" und
verdeckt spaeter echte Warnungen. Vorschlag: eigener Hygiene-Durchgang
(`cargo fix` + gezieltes Entfernen/Annotieren toten Codes), nicht Teil dieser
Re-Verifikation.

**Noch offen in Phase 1:** das Akzeptanzkriterium **A2** (echter TUI-Lauf) — das
kann nur am realen Terminal abgehakt werden (Checkliste im Fahrplan).

### Hygiene-Durchgang (2026-10-07, abgeschlossen)

Die 45 Warnungen sind **vollstaendig behoben**; `cargo build --workspace
--all-targets` ist warnungsfrei, `cargo test --workspace` weiter gruen (318).
Vorgehen: 16 maschinell (`cargo fix`), der Rest per Entfernen toten Codes
(`AsyncBridge`, `jobs_dir`, `last_modified`, `strip_tags_and_extract_text`,
`short_reasoning`, `CANCEL`, `DEFAULT_CACHE_TTL`, `POLL`, zwei TUI-Felder,
`Response.remaining`), zwei Deprecations migriert (`rand::thread_rng` -> `rng`,
quick-xml `unescape_value` -> `normalized_value`) und zwei echte Fehler behoben:

- **`Alt+Home`/`Alt+End` waren toter Code** — die Arme standen UNTER den
  ungeschuetzten `Home`/`End`-Armen und griffen nie (dieselbe Shadowing-Klasse,
  die der Ctrl+Pfeil-Kommentar schon beschreibt; vgl. die frueheren Ctrl+Up/Down).
  Jetzt stehen sie davor und funktionieren.
- `render_frame_text` war `pub`, obwohl sein `App`-Argument privat ist — auf
  `fn` reduziert.

Bewusst geparkt (mit begruendetem `#[allow(dead_code)]`, kein Verhaltensrisiko):
`render_minimap`/`total_wrapped_lines` (Minimap noch nicht in `draw()`
verdrahtet) und `short_reasoning` (durch die Streaming-Reasoning-Ansicht
abgeloest).

**Weitere Funde** (Verhalten/Interface):

- Der `patch`-Parameter **`mode`** (Schema: `replace` | `patch`) wurde gelesen,
  aber **nie verwendet** — die Angabe war wirkungslos und kostete
  Schema-Tokens. **Erledigt (2026-10-08):** `mode` aus Schema und Code entfernt;
  der Patch-Pfad ist jetzt rein `old_string`/`new_string` (+ `replace_all`).
- `browser_tool.rs` baut in `cdp_navigate` einen `reqwest::Client`, nutzt aber
  `reqwest::get` (frischer Client je Aufruf) — der gepoolte Client liegt brach.
  Der Hygiene-Durchgang hat die Zeile nur stillgelegt (`_http`); das Aufraeumen
  (entfernen oder wirklich poolen) steht aus.

### Sandbox-Naht (2026-10-07, Schritt 1 von ADR 0018)

`crates/rijo-core/src/sandbox.rs` setzt die Naht: `terminal` (Vordergrund) und
`execute_code` fragen `sandbox.backend`, **wo** ihr Prozess laufen darf.

- `off` (Default) — Host-Ausfuehrung, unveraendert.
- `auto`/`bwrap` — bubblewrap auf Linux: System read-only, Workspace schreibbar,
  `/tmp`/`/dev`/`/proc` privat, Netz optional (`sandbox.network`). **Opt-in**, weil
  ein Default-Wrapper jede Ausfuehrung ungemessen veraendern wuerde.
- `box` — Ausfuehrung in **Rijo Box** (`box.v1 exec.run`). Der Client steht noch
  aus, deshalb **fail-closed**: es laeuft nie still auf dem Host (Integrationstest
  `the_box_sandbox_backend_fails_closed`).

Tool-Namen/-Schemas unveraendert (Cache-Invariante); das Box-Backend steckt ohne
Interface-Aenderung ein. Background unter Nicht-`off` wird mit klarer Meldung
**abgelehnt**, nicht still herabgestuft. Der Guard entscheidet weiter *ob* gefragt
wird, das Backend *was ein erlaubter Prozess beruehren darf*. Offen (Schritt 2):
der `box.v1`-Client, die host-crossing/guest-only-Klassifikation und die
Background-Abdeckung; Windows als eigenes ADR.

## AC. File-Toolset: Analyse und Bereinigung (2026-10-08)

Schritt 1 der Toolset-fuer-Toolset-Pruefung. Fuenf Tools
(`read_file`, `write_file`, `patch`, `search_files`, `delete_file`).

**Befunde (Wiring/Doc ↔ Code):**

- `write_file` erzwang das **Read-before-Edit nicht**, obwohl README/Tool-Text es
  zusicherten — ein ungesehenes existierendes File wurde still ueberschrieben.
- `patch` bewarb den Parameter **`mode`** (`replace` | `patch`), las ihn und
  verwarf ihn — Toter Parameter, kostete Schema-Tokens.
- `search_files` bewarb **`files_only`**, implementierte es aber nicht (fiel auf
  content zurueck); war **immer** case-insensitive; `limit` wirkte nur *pro Datei*.
- `read_file` las fuer ein 10-Zeilen-Fenster die **ganze** Datei in einen String,
  brach bei **Nicht-UTF-8** mit Fehler ab und konnte bei `offset` hinter dem
  EOF durch `offset + line_count - 1` **unterlaufen** (Panic-Risiko).

**Aenderungen — Pass 1 (Verhalten, daher mit Tests):**

- `write_file`: Read-before-Edit wie `patch` — ein existierendes, nicht gelesenes
  File wird **abgelehnt**; ein selbst geschriebenes bleibt editierbar
  (`mark_read` nach Write). Deckt sich jetzt mit der Doku.
- `patch`: `mode` aus Schema **und** Code entfernt.
- `search_files`: `files_only` via `--files-with-matches` (Fallback zaehlt
  Dateien), **`case_sensitive`** und **`include`** (Glob) neu im Schema und in
  der rg-**und** Fallback-Kette; `limit` ist jetzt ein **hartes Gesamtlimit**.
- `read_file`: **streamendes, gebundenes** Lesen (eine Zeile zur Zeit statt
  ganzer Datei im Speicher), **lossy** bei Nicht-UTF-8 statt Fehler,
  `truncated` mit `saturating_sub` (kein Unterlauf), `total_lines`/`file_size`
  aus dem Stream.
- Kleinigkeit: No-op `let _ = &create_unified;` entfernt.

**Aenderungen — Pass 2 (alle offenen Luecken geschlossen):**

- **Multi-Edit**: `patch` nimmt jetzt `edits: [{old_string, new_string,
  replace_all?}]` — mehrere unabhaengige Aenderungen in **einem** Call, die
  **all-or-nothing** angewendet werden (schlaegt ein Block fehl, bleibt die Datei
  unberuehrt). Einzel-Edit (`old_string`) bleibt unveraendert. Die Match-Logik
  liegt in `apply_one_edit()` (eine Quelle fuer beide Formen).
- **Atomares Schreiben**: neuer Helfer `atomic_write()` (Temp-Datei im selben
  Verzeichnis + Rename, Modus wird uebernommen) fuer `write_file` **und**
  `patch`. Ein Leser/Crash sieht nie eine halb geschriebene Datei; kein
  `.rijo-tmp-*` bleibt liegen.
- **`delete_file` rekursiv**: `recursive: true` loescht einen **Verzeichnisbaum**
  (sonst wird ein Verzeichnis mit klarer Meldung abgelehnt). Approval-Gate
  unveraendert; zusaetzlich verweigert der Tool einen rekursiven Delete, wenn der
  Baum eine **`.env`** enthaelt (sonst waere die `.env`-Politik umgehbar).
- **Secret-Dateien**: `trust::is_env_file` schuetzt jetzt auch **`.env.*`**
  (`.env.local`, `.env.production`, …) vor Lesen/Loeschen. Templates
  (`.env.example`, `.env.sample`, `.env.template`, `.env.dist`) bleiben lesbar.
- **`brain_has_file()`-Stub entfernt** (lieferte immer `false`); `already_read`
  traegt allein `was_read`.
- **`search_files`-Fallback**: Skip-Liste um `dist`, `build`, `vendor`,
  `__pycache__`, `coverage` erweitert (rg ist die bevorzugte Engine und
  respektiert `.gitignore`; der Fallback wird ehrlich als „ohne .gitignore"
  dokumentiert).

**Tests:** 11 neue Integrationstests in `tool_contracts.rs` (Pass 1: Refuse-unread-
Overwrite, Rewrite nach eigenem Write, `files_only`, `case_sensitive`, Paging +
Nicht-UTF-8 + Offset-hinter-EOF; Pass 2: Batch-Edit, Batch-All-or-Nothing,
atomarer Write ohne Temp-Rest, `.env.*`-Schutz, rekursives Delete inkl.
`.env`-im-Baum-Ablehnung) + ein Unit-Test fuer `is_env_file` in `trust.rs`.
Gesamtsuite gruen (296 lib + 2 + 11 + 39 Integration), **0 Warnungen**
(`-D warnings`); `cargo build --workspace` sauber.

**Ehrlich offen:** Der `search_files`-Fallback parst **kein** `.gitignore` (nur
rg tut das). Alle anderen gefundenen File-Toolset-Luecken sind geschlossen.

## AD. Terminal-Toolset: Analyse und Bereinigung (2026-10-08)

Schritt 2 der Toolset-fuer-Toolset-Pruefung. Zwei Tools (`terminal`,
`process_manage`) plus die Registry (`process.rs`) und die Sandbox-Naht.

**Befunde:**

- **`pty` war ein toter Parameter** — im Schema beworben ("PTY for interactive
  CLIs"), als `_pty` durchgereicht, nie benutzt (dieselbe Klasse wie `patch.mode`).
- **Sudo-Gate mit Luecke** — `lower.starts_with("sudo ")` ohne `trim_start()`:
  ein fuehrendes Leerzeichen umging den Root-Approval.
- **`process::poll` Positions-Drift** — `out_pos += out.len()` nahm die
  **Lossy-String-Laenge** statt der gelesenen Bytes → bei Nicht-UTF-8 driftete
  die Leseposition und Bytes gingen verloren.
- **Timeout verwarf Teil-Ausgabe** — `"output": ""` und das `sandbox`-Feld fehlte
  (der Erfolgspfad hat es).
- **stderr nie gekappt** — nur stdout bei 50 000 Zeichen.
- **Doc-Luege (trust.rs + `features/security-env.md`)** — behauptete, das sealed
  env werde in terminal/Prozesse injiziert; `list_sealed()` wird nie benutzt.
- **`native_tool_redirect` Fehlalarme** — `tail -f` (Live-Log),
  `find … -delete/-exec` und `sed`/`awk` (Transformationen) wurden blockiert.
- **`timeout: 0`** erlaubt → sofortiger Kill (Schema ohne `minimum`).
- **`process::log` lud beide Logs ganz** in den Speicher; `poll`/`log`-Ausgabe
  war ungekappt; die **Registry wuchs unbegrenzt** (fertige Prozesse mit Handle).

**Aenderungen (Verhalten, daher mit Tests):**

- **`pty` entfernt** (Schema + Code). Interaktive CLIs laufen weiter ueber
  `background: true` + `process_manage write/submit` (stdin ist ein Pipe).
- **Sudo-Gate**: `trim_start()` vor der Erkennung.
- **`poll` byte-genau**: `read_slice` liefert die Rohbyte-Zahl; Positionen
  rücken darum korrekt vor.
- **Timeout behaelt Teil-Ausgabe** (stdout/stderr, gekappt) und setzt das
  `sandbox`-Feld.
- **Beide Streams gekappt** (`cap_output`, 50 000 Zeichen) — stdout **und** stderr.
- **Docs korrigiert**: sealed env wird **bewusst nicht** in die Shell injiziert
  (modellgetrieben → sonst per `env`/`echo` auslesbar); nur websurfx/search-URL
  und sudo-Passwort lesen `get_sealed`.
- **`native_tool_redirect` praezisiert**: `tail -f/-F/--follow` laeuft,
  `find` mit `-exec/-delete/-ok/-print0` laeuft, `sed -i` → `patch`,
  `sed` (druckend) und `awk` laufen.
- **Schema-Grenzen**: `timeout` min 1 / max 600 (terminal), `wait` min 1,
  `offset`/`limit` min 1 (process_manage).
- **`process::log` streamt** (eine Zeile zur Zeit, gebundener Speicher);
  `process_manage` kappt `poll`/`log`-Ausgabe; neues **`prune`** entfernt fertige
  Prozesse aus der Registry.

**Tests:** +4 Unit (Prozess-`read_slice`-Byte-Zaehlung, `prune`, zwei
`native_tool_redirect`-Faelle) und +2 Integration (Leerzeichen-Sudo-Gate,
Timeout-Teil-Ausgabe + `sandbox`-Feld). Gesamtsuite gruen
(300 lib + 2 + 11 + 41 Integration), **0 Warnungen** (`-D warnings`);
`cargo build --workspace` sauber.

**Bewusst offen:** Ein echtes **PTY** (fuer CLIs, die ein `isatty()` erzwingen)
bleibt ein spaeteres Feature — es braucht eine plattformuebergreifende
PTY-Abstraktion (neue Abhaengigkeit). Bis dahin ist `background`+stdin der
dokumentierte Weg; der tote `pty`-Schalter ist entfernt, damit das Schema nicht
luegt.

## AE. web-Toolset: Analyse und Bereinigung (2026-10-08)

Schritt 3 der Toolset-fuer-Toolset-Pruefung. Zwei Tools (`web_search`,
`web_extract`), die Provider-Kaskade und der Rate-Limiter.

**Befunde:**

- **`web_extract` Truncation panikte bei Nicht-ASCII** — `&cleaned[..half]`
  schnitt **Bytes**, nicht Zeichen → `byte index is not a char boundary` bei
  jedem Umlaut/CJK/Emoji, also auf fast jeder echten Seite.
- **`urlencoding` kodierte falsch** — `%{:02X}` auf den Code Point: `ü` → `%FC`
  (Latin-1) statt `%C3%BC`; `中` → `%4E2D` (ungueltig). Nicht-ASCII-Suchen kaputt.
- **`RateLimiter::note_retry_after` wirkte verkehrt** — `last_refill = now -
  seconds` (Vergangenheit) liess `refill` sofort die volle Kapazitaet
  wiederherstellen; der Retry-After-Backoff tat nichts.
- **`web_extract` pruefte den HTTP-Status nicht** — 404/500-Bodies wurden als
  „content" extrahiert.
- **Provider-`description` ungekappt** — Snippets konnten den Kontext fluten.
- **Extract ohne Groessen-Cap** und **Wikipedia ohne Provider-Timeout**
  (nutzte die 20 s des Clients).
- **`char_limit`** ohne Maximum im Schema.

**Aenderungen (Verhalten, daher mit Tests):**

- **`truncate_middle`** schneidet jetzt nach **Zeichen** (head + tail, mit
  Hinweis); keine Byte-Boundary-Panik mehr.
- **`urlencoding`** prozent-kodiert die **UTF-8-Bytes**.
- **`note_retry_after`** schiebt die Refill-Uhr in die **Zukunft** → echter
  Backoff.
- **`web_extract`**: HTTP-Status wird geprueft (`HTTP 4xx/5xx` als Fehler),
  PDF-Erkennung case-insensitiv, Groessen-Cap (5 MiB, Content-Length +
  Nachpruefung).
- **`description`-Cap** (`cap_hit`, 400 Zeichen + Ellipse), einheitlich in
  `answer()` fuer **alle** Provider.
- **Wikipedia** nutzt das Provider-Timeout; **`char_limit`** max 100 000.

**Tests:** +5 Unit (URL-Encoding UTF-8, `truncate_middle` zeichen-sicher inkl.
Umlaut + CJK, `note_retry_after` pausiert Refill, `cap_hit`-Kapazitaet,
`extract` meldet HTTP-404 statt Inhalt). Gesamtsuite gruen
(305 lib + 2 + 11 + 41 Integration), **0 Warnungen** (`-D warnings`);
`cargo build --workspace` sauber.

**Bewusst offen:** `web_extract` hat keinen eigenen per-URL-Timeout ueber den
Client-Default (20 s) hinaus und keinen `web_extract`-Cache; beides ist bisher
kein gemessenes Problem. SSRF auf interne URLs ist kein *neuer* Hebel — der
Agent hat ohnehin `terminal`.

## AF. Web-Extraktion: Readability (P1) (2026-10-08)

Erster Umsetzungsschritt aus `docs/web-search-plan.md`. Vorher lief die **ganze**
Seite (Nav, Cookie-Banner, Footer) durch `nanohtml2text`; der „Titel" war die
erste Zeile des Dumps, und Boilerplate dominierte kurze Seiten.

**Aenderung:**

- **`web_extract` extrahiert den Artikel** mit Mozilla-Readability
  (`dom_smoothie` 0.18, MIT) und liefert **Markdown**. Fallback auf
  Ganzseiten-Markdown (`htmd` 0.5, Apache-2.0), wenn eine Seite nicht lesbar ist.
- Ergebnis traegt jetzt: `title`/`byline`/`site_name`/`published_time`/`excerpt`
  (aus HTML/OpenGraph/JSON-LD), `readability` (welcher Pfad), `char_count` und
  opt-in `passages` (Absaetze, default aus → kein Token-Kosten).
- Der Fallback ist **ehrlich geflaggt** (`readability: false`), nie still.

**Messung (reproduzierbar):**

- Offline-Fixture (Unit-Test, `--nocapture`): Ganzseite 886 Zeichen (Boilerplate
  behalten) → Readability **687 Zeichen**, Cookie-Banner/Nav/Footer **weg**
  (Ratio 78 % = Artikelkoerper).
- Live (`--ignored`), Wikipedia „Rust (programming language)":
  **196 706 → 20 046 Zeichen** (`readability: true`, korrekter Titel). Eine
  Listing-Seite (`blog.rust-lang.org`) → **42 368 → 20 046** (`readability:
  false`, Fallback, ehrlich geflaggt).

**Tests:** +2 Offline (Readability schlaegt Ganzseiten-Text; Fallback geflaggt)
und +1 `#[ignore]`-Live-Messung (`cargo test -- --ignored --nocapture
live_article`). Gesamtsuite gruen (307 lib + 2 + 11 + 41 Integration),
**0 Warnungen** (`-D warnings`); `cargo build --workspace` sauber.

**Bewusst offen (naechste Phasen der Roadmap):** P2 Ranking (RRF-Fusion +
`fastembed`-Rerank, Provider-Ergebnisse aus Exa/Tavily agent-gerecht),
P3 Fetch-Robustheit (`governor`, `reqwest-retry`, `http-cache-reqwest`,
`texting_robots`, JS-Eskalation via vorhandenem CDP, PDF), P4 `answer:true` mit
Citations. Die `passages` existieren bereits als Baustein fuer den Reranker.

## AG. Web-Ranking: RRF-Fusion + lokaler Rerank (P2) (2026-10-09)

Zweiter Schritt aus `docs/web-search-plan.md`. `web_search` kann jetzt mehrere
Provider **fusionieren** statt „erster Treffer gewinnt".

**Aenderung:**

- **`web_search { deep: true }`**: fragt alle verfuegbaren Provider **parallel**
  ab (Brave/Tavily/Websurfx/Firecrawl), dedupliziert nach **kanonischer URL**
  (Host ohne `www.`, Pfad ohne Trailing-Slash, Tracking-Parameter weg) und
  fusioniert die Rankings per **Reciprocal Rank Fusion** (k=60). Ein Treffer, den
  mehrere Provider liefern, steigt nach oben. Der Fast-Pfad (erster Treffer)
  bleibt Default.
- **Provenienz**: jeder Treffer traegt `provider`; das Ergebnis traegt `mode`
  (`fast`/`deep`), `provider` (`fused` bei deep) und `providers` (die
  beitragenden Namen, sortiert).
- **Lokaler Rerank** (`web.rerank`, **Default aus**): ein Cross-Encoder
  (`fastembed::TextRerank`, Modul `rerank.rs`) sortiert die fusionierten Treffer
  nach Relevanz zur Query. Laeuft in `spawn_blocking`, Modell pro Prozess
  gecacht; **fail-open** — fehlt das Modell, bleibt die RRF-Reihenfolge, und eine
  Note sagt „rerank unavailable (kept RRF order)". `web.rerank_model` default
  multilingual (`jinaai/jina-reranker-v2-base-multilingual`).
- **Cache-Schluessel enthaelt den Modus** — ein `fast`-Ergebnis wird nie fuer
  eine `deep`-Anfrage ausgeliefert.

**Tests:** +6 Unit (kanonische URL/Dedup; RRF bevorzugt geteilte Treffer;
`deep` fusioniert + dedupliziert + setzt `providers`; `deep` umgeht den
`fast`-Cache; zwei Rerank-Ordnungstests). Gesamtsuite gruen
(313 lib + 2 + 11 + 41 Integration), **0 Warnungen** (`-D warnings`);
`cargo build --workspace` sauber.

**Bewusst offen:** Der Rerank-Pfad ist **fail-open by construction**, aber nicht
live gegen ein echtes Modell gemessen (der Download gehoert nicht in CI). Der
Provider **Exa** (Best fuer Research: `type: deep`, `highlights`, `summary`) ist
noch nicht angebunden — Kandidat fuer P2b. Eine Fusion/Rerank-Messung ueber
`--web-eval` fehlt noch.

## AH. `--web-eval`: Messwerkzeug + erster Messbefund (2026-10-09)

P0 aus `docs/web-search-plan.md`: ohne Messung bleibt „besser" eine Behauptung.

**Neu: `rijo --web-eval`** (`crates/rijo-core/src/web_eval.rs`). Fuehrt einen
festen, diversen Query-Satz (12; ueberschreibbar mit `--web-eval-queries FILE`)
durch die **echten** Tools und protokolliert:

- Latenz je Query in `fast` und `deep` (Reihenfolge **alterniert** pro Query, weil
  Provider-Caches den jeweils zweiten Aufruf bevorzugen),
- Trefferzahl + antwortende Provider (`providers` bei Fusion, sonst
  `(fallback:<name>)`),
- optional (`--web-eval-extract`) `web_extract` auf das Top-Ergebnis:
  `readability`-Pfad und Zeichenzahl,
- **kalt**: vor dem Lauf werden die `search_cache:`-Zeilen geleert (neue
  `BrainDb::kv_cache_delete_prefix`), sonst liefert ein alter Lauf ~0 ms.

Kein Ground-Truth-Relevanz-Scoring — bewusst, denn ein selbst erdachtes Set
waere manipulierbar. JSON-Dump nach `<data_dir>/web_eval/`.

**Erster Befund (gemessen, dieses Netz):** Der **keyless Firecrawl**-Schritt ist
hier **blockiert** — HTTP 403 „your IP address looks suspicious … can't be used
without an API key". Der Eval macht das sichtbar (`fast_note`, dann
`skipped (cooldown)`). Folge: ohne Key/Tavily/Brave/Websurfx degradiert die Suche
auf **Wikipedia**, und `deep` hat nichts zu fusionieren
(`deep_providers = (fallback:wikipedia)`). Die Kaskade verhaelt sich korrekt
(ehrlicher Fallback), aber die Zero-Config-Qualitaet haengt damit an Firecrawl,
das unzuverlaessig ist — Argument fuer einen **SearXNG** oder **Exa/Tavily-Key**
in P2b/P3.

**Tests:** +3 Unit (Median, Default-Query-Satz nicht leer/duplikatfrei,
`hit_stats`/`first_url`). Gesamtsuite gruen (316 lib + 2 + 11 + 41 Integration),
**0 Warnungen** (`-D warnings`); `cargo build --workspace` sauber.

**Bewusst offen:** Der Eval ist netzabhaengig (kein CI-Lauf). Ein groesserer,
gelabelter Query-Satz und eine Fusion/Rerank-Auswertung mit ≥2 echten Providern
(Key oder SearXNG) stehen aus.

## AI. Provider: Exa, SearXNG first-class, eingeschweisstes DuckDuckGo (2026-10-11)

Antwort auf den Eval-Befund aus §AH: die keyless Kette brauchte echte Breite,
und ein zweiter Provider fuer `deep`-Fusion.

**Aenderungen:**

- **Exa** (`search_exa`): keyed API, `x-api-key`-Header, `type: auto`,
  `contents.highlights` → die **Beschreibung** ist Exas LLM-gewaehlter
  Relevant-Schnipsel; `publishedDate` wird als `published` mitgegeben. In Fast-
  **und** Deep-Kette. Endpoint uebersteuerbar (`web.exa_api`).
- **SearXNG first-class**: Provider-Label jetzt **`searxng`** (vorher
  `websurfx`). URL-Aufloesung: config `web.searxng_url` → `SEARXNG_URL` →
  `SEARXNG_URLS` (erste) → historisch `WEBSURFX_URL` (Alias, Kompatibilitaet).
  Weiterhin **kein** Auto-Klon/Build (ADR 0007).
- **Eingeschweisstes, keyless DuckDuckGo** (`crates/rijo-core/src/meta_search.rs`):
  fragt **DDG Lite** direkt ab und parst das HTML mit zwei Regexen
  (`a.result-link` + `td.result-snippet`; die echte URL steckt im `uddg`-Query-
  Parameter und wird entpackt). **Keine neue Dependency**, kein Dienst, kein Key,
  keine JS/Captcha-Behandlung. Faellt bei jeder Seiten-Aenderung auf `Ok(vec![])`
  zurueck (die Kaskade geht weiter) — nie ein falsches Ergebnis. Endpoint
  uebersteuerbar (`web.ddg_api`). **Mojeek wurde geprueft und verworfen**: es
  antwortet Nicht-Browsern mit einem JS-Captcha (`altcha`).

**Kaskade jetzt:** Brave → Tavily → Exa → SearXNG → **DuckDuckGo (eingebaut)** →
Firecrawl → Wikipedia (fast) bzw. dieselben Provider per RRF fusioniert (deep).

**Tests:** +3 Unit im `meta_search`-Parser (Titel/URL/Snippet inkl. Redirect-
Entpacken, `limit`, geaenderte Seite → 0 Treffer) und +2 Integration (Exa: Key im
Header, Highlight als Beschreibung; eingebautes DDG antwortet keyless und packt
die URL aus). Gesamtsuite gruen (321 lib + 2 + 11 + 41 Integration),
**0 Warnungen** (`-D warnings`); `cargo build --workspace` sauber.

**Live-Befund (`--web-eval`, dieses Netz):** Das eingebaute DuckDuckGo
**funktioniert** (5 echte Treffer korrekt geparst, korrekte Struktur) — aber DDG
**drosselt schnell**: der erste Aufruf antwortet, der unmittelbar folgende (deep)
wird blockiert → Cooldown → der Rest faellt auf Wikipedia zurueck
(`fast=[duckduckgo, wikipedia]`). In reiner Fast-Nutzung (ein Aufruf je Query,
durch den Rate-Limiter getaktet) ist es brauchbar, aber nicht verlaesslich. Das
ist die ehrliche Grenze des Keyless-Scrapings.

**Bewusst offen:** SearXNG-**Multi-Instanz-Failover** (`SEARXNG_URLS` nutzt bisher
nur die erste URL) und ein **provider-spezifischer, kuerzerer Cooldown** fuer DDG
waeren die naechsten Hebel. Exas `type: deep`/`summary`/`additionalQueries` sind
noch nicht angesteuert. Verlaessliche Keyless-Breite bleibt SearXNG
(self-hosted) oder ein Key — genau wie der Eval nahelegt.

## AJ. Keyless-Robustheit: DDG-Cooldown + SearXNG-Failover (2026-10-11)

Die beiden in §AI als naechster Hebel genannten Punkte.

**Aenderungen:**

- **Provider-spezifischer Cooldown**: `effective_cooldown(name, configured, ddg)`
  — das **gescrapte** DuckDuckGo bekommt `web.ddg_cooldown_secs` (Default **60 s**)
  statt der globalen `search_cooldown_secs` (Default 900 s). Ein throttelndes DDG
  erholt sich damit innerhalb einer Sitzung, statt fuer 15 Minuten gesperrt zu
  bleiben. Keyed APIs und SearXNG behalten den globalen Wert.
- **SearXNG-Multi-Instanz-Failover**: config `web.searxng_urls` (Liste) plus
  `SEARXNG_URLS` (kommagetrennt) werden mit `SEARXNG_URL`/`WEBSURFX_URL`/
  `web.searxng_url` zu einer **geordneten, deduplizierten** Liste
  (`searxng_urls(state)`); `search_searxng_failover` fragt sie der Reihe nach ab —
  die erste Instanz mit Treffern gewinnt. Genutzt von Fast-Kaskade **und**
  Deep-Fusion.

**Tests:** +3 Unit (Failover auf die naechste Instanz; Liste geordnet/dedupliziert;
DDG bekommt den kuerzeren Cooldown). Gesamtsuite gruen
(324 lib + 2 + 11 + 41 Integration), **0 Warnungen** (`-D warnings`);
`cargo build --workspace` sauber.

**Bewusst offen:** Die Cooldown-Wirkung ist **nicht live gemessen** — sie zeigt
sich erst ueber Minuten (innerhalb eines `--web-eval`-Laufs bleibt DDG ohnehin
gedrosselt). DDG bleibt best-effort; der belastbare keyless Weg ist eine eigene
SearXNG-Instanz (jetzt mit Failover).

## AK. Rueckbau: web_search nur noch DuckDuckGo Lite (2026-10-11)

Entscheidung des Nutzers: **erstmal nur DDG Lite**, „alles andere raus" — eine
eigene Websuche-Engine in Rust kommt spaeter. Die §§AG–AJ (RRF-Fusion, `deep`,
lokaler Rerank, Exa/SearXNG/Firecrawl/Wikipedia, `--web-eval`) sind damit
**ueberholt**.

**Entfernt:**

- Provider **Brave, Tavily, Exa, SearXNG, Firecrawl, Wikipedia** und die ganze
  Mehrprovider-Maschinerie: Kaskade, `try_provider`, **RRF-Fusion**
  (`rrf_fuse`, `canonical_url`), `deep`, Provenienz (`mode`/`providers`),
  `gather_providers`.
- **Lokaler Rerank**: Modul `rerank.rs` (fastembed `TextRerank`).
- **`--web-eval`**: Modul `web_eval.rs`, die CLI-Flags, `BrainDb::kv_cache_delete_prefix`.
- **`websurfx.rs`** (Daemon-Lifecycle: Clone/Build/Spawn/Sandbox) und der
  Auto-Start in `executor.rs`; alle `websurfx::shutdown()`-Aufrufe in `main.rs`.
- Config: `web.brave_api`, `tavily_api`, `exa_api`, `firecrawl_api`,
  `wikipedia_api`, `searxng_url`, `searxng_urls`, `autostart_websurfx`,
  `rerank`, `rerank_model`, `ddg_cooldown_secs`, `secret()`, `urlencoding`.

**Geblieben:**

- **`web_search`** = das **eingebaute, keyless DuckDuckGo Lite** (`meta_search.rs`,
  unveraendert): ein gepoolter HTTP-Client, Rate-Limiter, Timeout, ein kurzer
  Cooldown (Default jetzt 60 s direkt als `web.search_cooldown_secs`) und der
  24h-Cache.
- **`web_extract`** unveraendert: Readability-Extraktion (`dom_smoothie` + `htmd`),
  Metadaten, opt-in `passages` — der Qualitaetskern aus §AF bleibt.
- Config reduziert auf `web.{search_timeout_ms, search_cooldown_secs,
  cache_ttl_secs, extract_parallel, ddg_api}`.

**Tests:** Die Provider-/Fusion-/Rerank-/Eval-Tests entfielen mit dem Code; die
`meta_search`-Parser-Tests und die Extraktions-/Cache-/DDG-Tests bleiben.
Gesamtsuite gruen (**301 lib** + 2 + 11 + 41 Integration), **0 Warnungen**
(`-D warnings`); `cargo build --workspace` sauber.

**Bewusst offen:** Der eingebaute `meta_search` ist bewusst klein (DDG Lite, ein
Regex-Parser, fail-open). Er ist der Platzhalter, bis die eigene Rust-Engine
steht; die Provider-Anbindung (Schluessel, Kaskade, Rerank) kann dann neu
aufgebaut werden.

## AL. web_search: flache Keyless-Provider-Liste + Per-URL-Cache (2026-10-11)

Antwort auf ADR `0022` (Rijoda-HQ) und den Messbefund aus §AK: `web_search` hatte
genau **einen** Provider — und das war der eine, der gemessen tot ist (DDG Lite
antwortet aus diesem Netz mit **202/Challenge**, 6/6, auch fuer einen echten
Headless-Browser).

**Aenderung (ADR 0022, Schritte 1+2):**

- **Neues Modul `crates/rijo-core/src/web_search.rs`** — eine *flache*
  Provider-Liste (`const PROVIDERS`): keine Kaskade, kein Router, kein Framework.
  Jeder Provider ist eine kleine Funktion mit **derselben Ausgabeform**
  `{title, url, description}`; die Parser (`parse_*`) sind getrennt und damit ohne
  Netz testbar. Provider (alle keyless, alle am 2026-10-11 live als `200`
  gemessen): **Wikipedia, crates.io, MDN, Stack Exchange, npm, grep.app,
  OpenAlex, Hacker News, Wiby** — plus das eingebaute DuckDuckGo als
  *best-effort* (letzte Prioritaet).
- **Parallel + Merge nach kanonischer URL.** Der Merge rankt nach: wie viele
  Provider sich einig sind → **Tier** (der #1 eines Providers schlaegt dessen #3)
  → statische **Quellen-Prioritaet** (Code/Doku vor Enzyklopaedie vor Community
  vor Scrape) → kanonische URL (nur fuer Determinismus). Jeder Treffer traegt
  `provider` (Erstzahler in `PROVIDERS`-Reihenfolge) und `sources` (alle,
  sortiert); die Antwort traegt `providers` (alle Beitragenden, sortiert).
- **Per-URL-Content-Cache** (`url_cache:*` im vorhandenen SQLite-`kv_cache`),
  Schluessel = kanonische URL + `char_limit` + `passages`. Ein zweiter Blick auf
  dieselbe Seite kostet **0 Netz**; ein Cache-Treffer wird als `cached: true`
  markiert. Nur echter Inhalt wird gespeichert, nie eine Fehlerseite.
- **`meta_search` ehrlich gemacht:** nur noch HTTP **200** gilt als Antwortseite.
  Die 202-Anti-Bot-Seite ist jetzt ein **Fehler** und wird im `note` benannt —
  vorher verschwand sie stillschweigend als „0 Treffer".
- **Config:** `web.endpoints` (Provider → Basis-URL) fuer Tests/Spiegel;
  `web.ddg_api` bleibt als historischer Einzel-Override erhalten.
- **Dependency:** `reqwest` bekommt die `gzip`-Feature (sonst kommt eine
  gzip-Antwort als Binaermuell an).

**Messung (reproduzierbar):**

- Live (`cargo test -p rijo-core -- --ignored --nocapture live_search`):
  - `rust async http client`: **870 ms**, 5 Treffer, **9 Beitragende**
    (crates-io, duckduckgo, hn, mdn, npm, openalex, stackexchange, wiby, wikipedia).
  - `recursive rank fusion`: **851 ms**, 8 Beitragende.
  - `tokio task spawn blocking`: **1733 ms**, 9 Beitragende.
  - Vorher (DDG-only) waren es von diesem Netz **0 Treffer**. DuckDuckGo bleibt
    eine Lotterie: hier lieferte es echte Treffer, in der HQ-Messung 202.
- Live (`live_extract_cache`): dieselbe Wikipedia-Seite zweimal →
  **1608 ms** (163 900 Zeichen, `readability: true`), dann **0 ms** und
  `cached: true` mit identischem Inhalt.
- `grep.app` limitiert schnell (`HTTP 429`); es wird im `note` benannt, die Suche
  laeuft weiter (fail-open).

**Tests:** +15 Unit in `web_search` (9 Parser-Fixtures aus echten Antworten,
`canonical_url`, Konsens-Praeferenz, deterministische Tie-Ordnung) und
+2 Integration („zwei Provider werden gemerged und benannt", „eine URL wird
einmal extrahiert, dann aus dem Cache bedient") sowie zwei `#[ignore]`-Live-
Messungen. Gesamtsuite gruen (**316 lib** + 2 + 11 + 41 Integration),
**0 Warnungen**; `cargo build --workspace` sauber.

**Bewusst offen (ehrlich):** Der Merge bringt **Breite, nicht semantische
Relevanz** — ohne Konsens entscheidet die Quellen-Prioritaet, deshalb steht fuer
`rust async http client` `crates.io/crates/reqres` vorn (ein Namens-Treffer der
crates.io-Suche), obwohl `reqwest`/`docs.rs` naeher laegen. Ein Kreuz-Encoder-
Rerank oder ein `objective`-Parameter waeren der naechste Hebel; beides ist in
`0022` bewusst vertagt. Kein Provider braucht einen Schluessel — allgemeine
Web-Breite und Frische brauchen weiterhin einen Key (Schritt 3).

