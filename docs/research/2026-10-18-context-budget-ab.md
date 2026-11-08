# Context-budget defaults: A/B measurement

**Date:** 2026-10-18

**Question:** do tighter context defaults actually reduce the prompt volume, and by how
much? The trigger was a real desktop turn that showed "335k context tokens" after two
prompts — which turned out to be the **cumulative billed** prompt tokens, not the context
fill (see `docs/features/browser-devtools.md` and the fixed `last_prompt_tokens`).

## Setup

Same machine, same model (`deepseek-flash`), same workspace (`C:\Users\Mo\Documents\rijoda\site`),
same prompt (`analysiere die website`), `learning.after_turn: false` in both arms to remove
review cost and variance. The CLI ran in one-shot mode; both arms used the same binary.

| Arm | `tool_result_cap` | `tool_stub_after_messages` | `router.max_tools` |
|---|---|---|---|
| Old | 8000 | 8 | 8 |
| New | 6000 | 6 | 6 |

Note: `router.max_tools` was a **silent no-op** until this change — the router always used
the ONNX bundle's value. The new arm below therefore ran with the field actually wired
(`RouterSession::set_max_tools`).

## Results (one run per arm)

| Metric | Old | New | Δ |
|---|---|---|---|
| iterations | 18 | 8 | −10 |
| tool calls | 27 | 12 | −15 |
| **prompt tokens billed** | **182,189** | **72,799** | **−60.0 %** |
| last request (context) | 11,258 | 11,264 | ±0 |
| completion tokens | 5,681 | 4,675 | −17.7 % |
| total time | 41.9 s | 28.4 s | −32 % |
| failures | 1 | 0 | −1 |
| schema sent | 13 tools / 7,986 chars | 11 tools / 7,371 chars | −7.7 % chars |

Per-request estimates from `diagnostics.log` stayed in the same band: **3.8–4.8k** (old)
vs **4.5–6.3k** (new). The caps bound the worst case; they do not force a smaller request
when the model reads bigger files.

## Reading, honestly

- **The 60 % is not attributable to the config alone.** The model took a very different
  path (12 vs 27 tools, 8 vs 18 iterations). The caps and the schema cap explain a
  **~8 % smaller schema** and a lower worst case per tool result (6k vs 8k chars ≈ 500
  tokens), i.e. roughly **1–3k tokens per request** — not 110k.
- **The main lever is the number of iterations/tool calls**, which is model behaviour and
  task scope. The same workspace produced 27 tools in the old arm and 12 in the new one.
- **The context was never the problem.** The real context stayed around 4–11k; the 335k in
  the desktop was the cumulative billed sum over 22 requests (now labelled "billed").
- `router.max_tools` is now real: 8 predicted → 6 predicted, sent 13 → 11 tools, but the
  always-keep list (7 tools) dominates the schema. Lowering it further has little return.

## Decision

Keep the new defaults: `6000 / 6 / 6`. They lower the worst case, trim the schema a little,
and make the router cap honest. They do not replace prompt discipline: batch reads, prefer
`search_files` over whole-file reads, and keep the task scope tight.

## Limits

- One run per arm, temperature 0.7. The path difference dominates; a 3×3 repetition would
  be needed to separate the config effect from model variance (cost: ~0.5M tokens).
- The site had been audited before; the learned `static-site-repo-audit` skill was present
  in both arms (it was not modified during the A/B because learning was off).
