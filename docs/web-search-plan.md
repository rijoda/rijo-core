# Web search & extraction — research and plan (Rust)

Status: proposal · Date: 2026-10-08 · Scope: the `web_search` / `web_extract`
toolset (`crates/rijo-core/src/tools/web_tools.rs`)

Progress (2026-10-11): **rolled back to a single built-in engine.** Per a user
decision, `web_search` is now only the keyless DuckDuckGo Lite
(`crates/rijo-core/src/meta_search.rs`); the multi-provider cascade, RRF fusion,
`deep`, the local `fastembed` reranker, Exa/SearXNG/Firecrawl/Wikipedia and the
`--web-eval` harness were **removed** (see `history/goal-100-de.md` §AK). A dedicated search
engine in Rust is planned separately.

**What survives from this plan:** **P1** — Readability extraction in
`web_extract` (`dom_smoothie` + `htmd`, metadata, opt-in passages) — is kept, see
`history/goal-100-de.md` §AF. Everything else in P0–P4 is superseded; the sections below are
kept as the (now historical) research.

Purpose: make the web tools *enormously good* — and name, with evidence, how we
get there in Rust. This is a research + plan document, not a change log. Every
crate fact below was read from the crates.io API on **2026-10-08**; nothing here
is benchmarked on our machine yet (that is the point of phase P0).

---

## 0. TL;DR — the five moves that matter

1. **Extract the *article*, not the page.** Today `web_extract` runs the whole
   HTML through `nanohtml2text`/`html2text`, so nav, cookie banners and footers
   are part of "content". Move to **`dom_smoothie`** (a maintained Rust port of
   Mozilla Readability) and emit **Markdown** (`htmd`). This is the single
   biggest quality lever.
2. **Rerank with a real cross-encoder.** We already ship `fastembed` (v5, ONNX);
   it exposes `TextRerank` (BGE / Jina rerankers). Fuse providers with RRF, then
   rerank top-K against the query. This is what separates "a list of links" from
   "the right passages".
3. **Make search multi-provider and *semantic*.** Keep the cascade, add **Exa**
   (`highlights`, `summary`, `objective`, `type: deep`) and **Tavily**'s
   agent-grade options (`include_answer`, `include_raw_content=markdown`,
   `time_range`, `topic: news`). First-class **SearXNG** for the keyless,
   self-hosted path.
4. **HTTP-first, browser-on-demand.** Reuse the CDP client we already have
   (`cdp-browser`); fetch with HTTP, escalate to headless Chrome only when the
   page is JS-rendered. `spider` does exactly this — study it, don't necessarily
   depend on it.
5. **Measure it.** A local eval set + a `--web-eval` harness (extraction ratio,
   main-content recall, p50/p95 latency, provider success, dedup rate). "Good"
   without numbers is not this repo's culture.

---

## 1. What "enormously good" means (definition + metrics)

For an LLM agent, a web tool is good when, per query and within a token/latency
budget, it delivers **the few passages that answer the query, with citable
sources, and nothing else**. Concretely, we should track:

| Dimension | Metric | Target (proposal) |
|---|---|---|
| Extraction quality | main-content chars / raw HTML chars; boilerplate ratio | article body ≥ 80 % of extracted text |
| Relevance | nDCG@k / hit-rate@k on a fixed query→expected-fact set | > Tavily/HN baseline on our set |
| Latency | p50 / p95 per `web_search`, per `web_extract` | p50 < 1.5 s cached, < 4 s cold |
| Reliability | provider success rate; fallback rate | ≤ 1 provider timeout per session |
| Economy | tokens returned per answer; cache hit rate | cached search ≈ 0 network |
| Fidelity | sources returned = sources cited; no invented URLs | 100 % |

---

## 2. Current state (grounded)

Already there (good, keep):

- Cascade with **per-provider timeout + cooldown** and a shared pooled
  `reqwest` client (`web_tools.rs:40-57`).
- SQLite-backed, TTL'd **search cache** whose key includes `limit`.
- Parallel `web_extract` (up to 5 URLs, `extract_parallel`).
- `secret()` reads env then the **sealed** `.env` (wiring regression already
  fixed).
- A CDP browser client (`browser_*`, `cdp-browser` feature) we can reuse.

Gaps (the reason it is not yet "enormously good"):

- **No readability/main-content extraction.** `extract_single_page` feeds raw
  HTML to `nanohtml2text::html2text` (`web_tools.rs`), then takes the first line
  as the "title". Boilerplate dominates short pages; titles are often wrong.
- **No reranking or fusion.** Provider order is the only ranking; descriptions
  are whatever the provider returns, capped at 400 chars by our recent fix.
- **No HTTP-level cache** (`ETag`/`Cache-Control`) — only our semantic cache.
- **No `robots.txt`, no retry/backoff middleware**; the hand-rolled token bucket
  had a real backoff bug (fixed), and `governor` would remove the class.
- **PDF extraction is a placeholder** ("use a PDF reader").
- **No JS rendering** for the *search/extract* path (the browser tools are a
  separate toolset the model must know to chain).
- **No answer synthesis with citations** (Tavily/Exa offer it server-side).

---

## 3. Rust ecosystem research (verified facts)

Versions/licenses/download counts read from crates.io on 2026-10-08.

### 3.1 Content extraction (HTML → article/markdown)

| Crate | Latest | Licence | Downloads (recent) | Note |
|---|---|---|---|---|
| **`dom_smoothie`** | 0.18.2 (2026-09) | MIT | 580k (429k recent) | Rust port of Mozilla **Readability**; `Article{title,byline,content,text_content,excerpt,published_time,…}`, **`TextMode::Markdown`**, JSON-LD metadata, `is_probably_readable()`, alternative candidate selection. Actively developed. |
| `readability` (kumabook) | 0.3.0 (**2023**) | MIT | 1.58M (445k) | arc90 port, **unmaintained**. |
| `readability-rust` (dreampuf) | 0.1.0 (2025-07) | — | 130k | Readability port, single release. |
| `readability-js` | 0.1.6 (2026-09) | — | 23k | Wraps Mozilla's Readability.js → needs a JS runtime (deno/V8). |
| **`htmd`** | 0.5.5 (2026-07) | Apache-2.0 | 5.88M (4.39M) | turndown.js-inspired HTML→Markdown. |
| `html2text` (we use 0.6) | 0.6 | MIT | — | Older; whole-page text. |
| `nanohtml2text` (we use 0.1) | 0.1 | — | — | Tiny; whole-page text. |
| `dom_query` | 0.28.0 (2026-05) | MIT | 17.5M | CSS-selector DOM query/manipulation (same author as dom_smoothie; used to post-process or custom-extract). |

**Pick:** `dom_smoothie` (extract) + `htmd` (markdown) as the primary path;
`dom_query` for site-specific tweaks. Keep the current converters only as a
no-Readability fallback.

### 3.2 Fetching, caching, politeness

| Crate | Latest | Licence | Downloads | Note |
|---|---|---|---|---|
| `reqwest` (have 0.12) | 0.12 | MIT/Apache | huge | Pooled client kept. |
| `http-cache-reqwest` | 0.16.0 (2026-09) | MIT | 3.80M (939k) | RFC-9111 HTTP cache middleware (ETag/Cache-Control). |
| `reqwest-middleware` / `reqwest-retry` | — | MIT | — | Idempotent retry + exponential backoff as middleware. |
| **`governor`** | 0.10.4 (2025-12) | MIT | **82.8M** (18.0M) | The canonical token-bucket rate limiter; keyed (per-domain). |
| `texting_robots` | 0.2.2 | MIT | 956k (378k) | Thoroughly tested `robots.txt` parser. |
| `moka` | (active) | MIT/Apache | huge | In-memory TTL+LRU cache (alternative/complement to SQLite). |
| `feed-rs` | (active) | Apache/MIT | — | RSS/Atom/JSON Feed parsing (for news queries). |

### 3.3 Ranking / embeddings (we already have the engine)

`fastembed` **v5 is already a dependency**. It supports:

- **`TextRerank`** with `RerankerModel::BGERerankerBase`, `BGERerankerV2M3`,
  `JinaRerankerV1TurboEn`, `JinaRerankerV2BaseMultilingual`.
- Dense embeddings (`EmbeddingModel::AllMiniLML6V2`, `BGESmallENV15`, `BGE-M3`,
  `jina-embeddings-v2-base-*`, multilingual-e5…), sparse (SPLADE, BGE-M3) and
  `similarity::{cosine_similarity, top_k}`.

So local **reranking and semantic dedup need no new engine** — only model
choices and wiring. Use the multilingual reranker (`jina-reranker-v2-base-
multilingual`) since the lab is German/English.

### 3.4 Crawling & JS rendering

| Crate | Latest | Licence | Downloads | Note |
|---|---|---|---|---|
| `spider` | 2.x | MIT | large | Concurrency-first crawler: **HTTP-first, launches headless Chrome only when needed** (`crawl_smart`), robots.txt, proxies, retries, streaming, Markdown/JSON/WARC export. Heavy dependency. |
| `headless_chrome` | 1.0.22 (2026-06) | MIT | 3.40M (776k) | Sync CDP driver. |
| `chromiumoxide` | 0.9.1 (2026-02) | MIT | 4.83M (2.08M) | Async CDP driver. |

We already drive CDP via `tokio-tungstenite` (`cdp-browser`). **Do not add
`spider` wholesale** unless crawling many pages becomes a real need; instead
mirror its *escalation policy* on our existing CDP client.

### 3.5 PDF

PDF extraction in Rust is the weakest area. Candidates: `pdf-extract` (0.10,
quality caveats) and forks (`anymd-pdf-extract`, `hygg-pdf-extract`),
`lopdf` (low-level), or `pdfium-render` (best fidelity, ships/needs the pdfium
binary — a packaging cost on Windows-GNU). Treat as **P3** and gate it behind a
feature so the base build stays dependency-light.

---

## 4. Provider / API landscape (what each gives an agent)

- **Brave** (have): its own index, strong relevance, plain results
  (`title/url/description`). Key in a header. Best first step.
- **Tavily** (have): agent-grade. Key options verified from its OpenAPI:
  `search_depth` (`basic|fast|advanced|ultra-fast`), `chunks_per_source` (1–3),
  `include_answer` (LLM answer), `include_raw_content` (`markdown|text`),
  `time_range`/`topic` (`general|news|finance`), `include_domains`/`exclude_domains`,
  `include_published_date`, `auto_parameters`, `exact_match`.
- **Exa** (consider): the strongest for *research* semantics; verified options:
  `type` (`instant|fast|auto|deep-lite|deep|deep-reasoning`), `contents.text`
  (`maxCharacters`, `verbosity`), `highlights` (LLM-picked relevant snippets,
  `{query}`-guided), `summary`, `outputSchema` (structured), `additionalQueries`,
  `objective`, `category` (`company|news|research…`), `start/endPublishedDate`.
- **SearXNG** (have as Websurfx-compatible): self-hosted, keyless, aggregates
  engines; JSON API. The honest "no key, no cloud" default (fits the lab thesis).
- **Firecrawl** (have, keyless): search + crawl + clean markdown; keyless tier
  works but is rate-limited.
- **Jina Reader** (`r.jina.ai/<url>`): zero-config URL→Markdown reader; useful as
  an extraction *fallback* when local extraction fails. (External service;
  document the privacy trade-off.)
- **Wikipedia** (have): the always-there last resort.

Design rule: **providers are a pluggable, ordered chain with per-provider
capability flags** (returns snippets? raw content? answers? news/freshness?).
The chain is data, not `if`-ladders.

---

## 5. Target architecture (the pipeline)

```
query
 │
 ├─ 0. Cache: semantic (SQLite) → HTTP (ETag) → in-memory (moka)
 │
 ├─ 1. Query understanding (cheap, local):
 │     normalize · detect language/news intent · optional rewrite
 │     (multi-query for deep mode) · domain hints
 │
 ├─ 2. Search — provider chain, bounded per-provider timeout + cooldown:
 │     Brave → Exa/Tavily → SearXNG → Firecrawl → Wikipedia
 │     each returns normalized {title,url,snippet,published,provider,score}
 │
 ├─ 3. Fuse + dedup:  RRF across providers · URL canonicalization ·
 │     near-duplicate collapse (title+host, optional embedding cosine)
 │
 ├─ 4. Fetch top-N (HTTP-first):  readability extract (dom_smoothie)
 │     ├─ if not readable / JS shell → escalate to headless Chrome (CDP)
 │     └─ if still empty → Jina Reader fallback
 │
 ├─ 5. Shape:  Markdown (htmd) · cap · JSON-LD/OG metadata (title, date, site)
 │
 ├─ 6. Rerank:  split into passages → fastembed TextRerank(query, passages)
 │     → keep the top passages per page and globally (token budget)
 │
 └─ 7. Answer:  return {results:[{title,url,published,passages[]}]}
        · optional `answer:true` → local LLM synthesis WITH citations
```

Invariants carried from the rest of the toolset: **fail-open** (a dead provider
is skipped and *named*), **bounded** (timeout, cooldown, size, token caps),
**deterministic** where it affects the prompt, **honest** (no invented URLs; the
`note`/`provider` fields already model this).

---

## 6. Concrete crate decisions

| Concern | Today | Proposal | Why |
|---|---|---|---|
| Main-content extract | `nanohtml2text`, `html2text` | **`dom_smoothie`** | Readability; the biggest quality lever |
| HTML→Markdown | `html2text` | **`htmd`** (or `dom_smoothie` `TextMode::Markdown`) | Turndown-equivalent, active |
| DOM tweaks | — | `dom_query` (only if needed) | CSS selectors, same author |
| HTTP cache | none | **`http-cache-reqwest`** + file store | ETag/Cache-Control |
| Retry/backoff | bespoke | **`reqwest-retry`** (+ middleware) | one well-tested class |
| Rate limit | bespoke bucket | **`governor`** (keyed per host) | canonical, keyed |
| robots.txt | none | **`texting_robots`** | polite crawling |
| Rerank | none | **`fastembed TextRerank`** (already a dep) | local cross-encoder |
| JS render | CDP browser (separate) | reuse CDP, HTTP-first escalation | no new dep |
| PDF | placeholder | `pdfium-render`/`pdf-extract` behind a feature | P3 |
| Feeds | none | `feed-rs` | news queries (P3) |

Dependency discipline: add **`dom_smoothie` + `htmd`** first (small, pure Rust),
then middleware/cache/governor behind the existing `full` feature; keep PDF and
feed support behind explicit features so the base binary stays lean.

---

## 7. Phased plan

**P0 — Eval baseline (must precede any "better").**
- Freeze a query set (≥ 40: factual, news, German, code, "find similar").
- Record per result: provider, latency, extracted chars, main-content ratio,
  whether the expected fact/URL appears.
- Add `rijo --web-eval` (mirrors `--schema-eval`) printing the table.

> **Partial (2026-10-09):** `rijo --web-eval` shipped (12-query default set,
> fast+deep, cold, optional extract, JSON dump; `history/goal-100-de.md` §AH). Still open:
> a larger labeled set and any relevance scoring.

**P1 — Extraction quality.**
- Swap `web_extract` to `dom_smoothie` (+ `htmd`), keep the current converters as
  fallback. Add `published_time`/`site_name`/byline to the result.
- Passages instead of one blob: split the article into ~paragraph chunks with
  positions; cap by chars (token budget).
- Tests: article page vs. nav-heavy page; ensure `<p>` text survives and the
  cookie banner does not.

> **Done (2026-10-08):** `dom_smoothie` + `htmd` in `web_extract`, metadata,
> `readability` flag, opt-in `passages`; fallback = whole-page Markdown. Measured
> in `history/goal-100-de.md` §AF (offline fixture + live Wikipedia).

**P2 — Ranking.**
- Normalize all providers to one hit shape; add **RRF fusion** + URL dedup.
- Add **`fastembed` `TextRerank`** to reorder fused hits and to pick the top
  passages per page. Measure delta on P0.
- Add Exa and Tavily's agent options; make the chain capability-driven.

> **Partial (2026-10-11):** `deep:true` RRF fusion + canonical-URL dedup +
> provenance (`mode`/`provider`/`providers`) done; local rerank wired
> (`web.rerank`, default off, fail-open, `rerank.rs`); **Exa** provider added;
> **SearXNG** first-class; built-in keyless **DuckDuckGo** (`meta_search`). Still
> open: per-page **passage** rerank and a fusion/rerank measurement with ≥2 real
> providers.

**P3 — Fetch robustness.**
- `governor` (per-host), `reqwest-retry`, `http-cache-reqwest`, `texting_robots`.
- JS escalation via the existing CDP client when extraction is empty/short.
- Jina Reader as a documented last-resort fallback.
- PDF behind a feature.
- Built-in keyless meta-search so the zero-config path has real breadth.

> **Partial (2026-10-11):** built-in keyless **DuckDuckGo Lite**
> (`meta_search.rs`) shipped — no service, no key, fail-open; Mojeek rejected
> (JS captcha). SearXNG **multi-instance failover** and a shorter **DDG cooldown**
> landed too. Still open: everything else above (retry/cache/robots/JS/PDF).

**P4 — Answers & research mode.**
- `web_search(answer:true)` → local LLM synthesis with **citation ids** bound to
  returned results (never invented).
- `research` mode: `deep` (Exa) / multi-query + iterative fetch-rerank, bounded.

Each phase: tests + a `history/goal-100-de.md` entry with the measured delta (this repo's
rule: no unmeasured claims).

---

## 8. Risks & honest open questions

- **Dependency weight vs. binary size.** `dom_smoothie` is ~3.4k LOC (fine);
  `spider` is heavy — avoid unless crawling is a real feature.
- **Reranker cost.** A cross-encoder adds CPU latency per call; must be measured
  (fastembed is ONNX/CPU here). Fall back to RRF-only when the model is absent.
- **JS escalation** needs Chrome present; must degrade cleanly (report
  "JS-rendered, no browser available") rather than return an empty shell.
- **External fallbacks (Jina, Firecrawl)** are third-party and rate-limited;
  they are *fallbacks*, never the primary, and the trace must say so.
- **Evaluation is the hard part.** A query set that we design can be gamed by
  ourselves; add held-out queries and, where possible, an independent reference
  (e.g., compare against Tavily's own answer on the same queries).
- **Licensing:** `dom_smoothie` MIT, `htmd` Apache-2.0, `governor` MIT,
  `http-cache-reqwest` MIT, `texting_robots` MIT — all compatible with
  `Apache-2.0`. Verify each at adoption time.

---

## 9. References (read 2026-10-08)

- crates.io API: `dom_smoothie`, `htmd`, `readability`, `readability-rust`,
  `chromiumoxide`, `headless_chrome`, `governor`, `http-cache-reqwest`,
  `texting_robots`, `dom_query`, `searxng-client`.
- `dom_smoothie` README (Readability port: `Article`, `TextMode::Markdown`,
  `is_probably_readable`, `CandidateSelectMode`, `ParsePolicy`).
- `fastembed-rs` README (`TextRerank` / `RerankerModel`, embedding models).
- `spider` README (HTTP-first + Chrome-on-demand `crawl_smart`, robots, streaming).
- Tavily Search OpenAPI; Exa Search OpenAPI (both fetched 2026-10-08).
