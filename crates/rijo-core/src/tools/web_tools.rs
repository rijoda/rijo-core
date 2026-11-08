//! Web tools — search and extract web page content
//!
//! `web_search` asks a **flat list of free, key-less providers in parallel**
//! (`crate::web_search`: Wikipedia, crates.io, MDN, Stack Exchange, npm,
//! grep.app, OpenAlex, Hacker News, Wiby, plus the built-in DuckDuckGo scrape as
//! best-effort) and merges them by canonical URL — ranked by how many providers
//! agreed. `web_extract` fetches pages in parallel and returns the main article
//! as Markdown (Mozilla Readability via `dom_smoothie`, whole-page fallback via
//! `htmd`), with a per-URL cache so a page is fetched once.
//!
//! Measured basis and decision: `docs/60-research/rijo-web-*` and ADR `0022`.
//! History: a keyed cascade (Brave/Tavily/Exa/SearXNG/Firecrawl) with RRF fusion
//! and a local reranker once lived here and was removed (`4f3cae9`); the flat
//! provider list above is the deliberate, smaller successor.

use crate::tools::{tool_error, AsyncTool, JsonSchema, ToolMeta, ToolResult};
use crate::AppState;
use async_trait::async_trait;
use rand::Rng;
use serde_json::{json, Value};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// The provider names the caller skips because they are cooling down.
fn providers_in_cooldown() -> Vec<&'static str> {
    let mut out = Vec::new();
    for (name, _, _) in crate::web_search::PROVIDERS {
        if provider_in_cooldown(name) {
            out.push(*name);
        }
    }
    out
}

// ─── HTTP client ──────────────────────────────────────────────────────────
//
// One pooled client for the whole process. A client per call discards the pool,
// so every request would pay DNS + TCP + TLS again.

const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36                           (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

lazy_static::lazy_static! {
    /// Shared, pooled HTTP client: keeps TCP/TLS sessions alive across calls.
    static ref HTTP: reqwest::Client = reqwest::Client::builder()
        .user_agent(BROWSER_UA)
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .unwrap_or_default();

    /// Until when the engine is skipped after a timeout/error. DuckDuckGo Lite is
    /// scraped and throttles, so without this every search would hammer it.
    static ref PROVIDER_SKIP_UNTIL: std::sync::Mutex<
        std::collections::HashMap<&'static str, std::time::Instant>,
    > = std::sync::Mutex::new(std::collections::HashMap::new());
}

fn provider_in_cooldown(name: &str) -> bool {
    PROVIDER_SKIP_UNTIL
        .lock()
        .ok()
        .and_then(|guard| guard.get(name).copied())
        .map(|until| std::time::Instant::now() < until)
        .unwrap_or(false)
}

fn note_provider_failure(name: &'static str, cooldown: Duration) {
    if let Ok(mut guard) = PROVIDER_SKIP_UNTIL.lock() {
        guard.insert(name, std::time::Instant::now() + cooldown);
    }
}

fn note_provider_success(name: &'static str) {
    if let Ok(mut guard) = PROVIDER_SKIP_UNTIL.lock() {
        guard.remove(name);
    }
}

/// Clear every cooldown. The map is process-wide, so tests must reset it.
#[cfg(test)]
fn reset_all_cooldowns() {
    if let Ok(mut guard) = PROVIDER_SKIP_UNTIL.lock() {
        guard.clear();
    }
}

lazy_static::lazy_static! {
    static ref RATE_LIMITER: RateLimiter = RateLimiter::new(10, 1);
}

struct RateLimiter {
    capacity: u64,
    refill_per_sec: u64,
    tokens: AtomicU64,
    last_refill: AtomicU64,
}

impl RateLimiter {
    fn new(capacity: u64, refill_per_sec: u64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: AtomicU64::new(capacity),
            last_refill: AtomicU64::new(now_secs()),
        }
    }

    async fn acquire(&self) {
        loop {
            refill(self);
            let cur = self.tokens.load(Ordering::SeqCst);
            if cur > 0
                && self
                    .tokens
                    .compare_exchange(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                return;
            }
            let jitter = 100 + rand::rng().random_range(0..200);
            tokio::time::sleep(Duration::from_millis(jitter)).await;
        }
    }
}

fn refill(l: &RateLimiter) {
    let now = now_secs();
    let last = l.last_refill.load(Ordering::Relaxed);
    let elapsed = now.saturating_sub(last);
    if elapsed > 0 {
        let refill = elapsed.saturating_mul(l.refill_per_sec);
        let new_tokens = (l.tokens.load(Ordering::Relaxed) + refill).min(l.capacity);
        l.tokens.store(new_tokens, Ordering::Relaxed);
        l.last_refill.store(now, Ordering::Relaxed);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

const DEFAULT_CACHE_TTL: u64 = 24 * 60 * 60;

/// The limit is part of the key: a `limit=5` answer must not be served for a
/// `limit=20` request.
fn query_hash(query: &str, limit: usize) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    query.trim().to_lowercase().hash(&mut hasher);
    limit.hash(&mut hasher);
    format!("search_cache:{:x}", hasher.finish())
}

fn cache_ttl(state: &AppState) -> u64 {
    match state.config.web.cache_ttl_secs {
        0 => DEFAULT_CACHE_TTL,
        n => n,
    }
}

fn cache_get(state: &Arc<AppState>, query: &str, limit: usize) -> Option<String> {
    let key = query_hash(query, limit);
    let (val, saved_at) = state.brain.kv_cache_get(&key)?;
    ((now_secs() as i64 - saved_at) < cache_ttl(state) as i64).then_some(val)
}

fn cache_put(state: &Arc<AppState>, query: &str, limit: usize, value: &str) {
    let key = query_hash(query, limit);
    state.brain.kv_cache_put(&key, value);
}

// ─── WebSearch-Tool ───────────────────────────────────────────────────────

pub struct WebSearch;

#[async_trait]
impl AsyncTool for WebSearch {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "web_search",
            description: "Search the web (built-in keyless DuckDuckGo). 24h cache.",
            emoji: "🔍",
            toolset: "web",
            is_async: true,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &WEB_SEARCH_SCHEMA
    }

    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let query = match args.get("query").and_then(|v| v.as_str()) {
            Some(q) if !q.trim().is_empty() => q.to_string(),
            _ => return tool_error("query is required"),
        };
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .min(100) as usize;

        // 1) Cache
        if let Some(cached) = cache_get(&state, &query, limit) {
            return cached;
        }

        // 2) Rate-limit (be polite to the scraped engine)
        RATE_LIMITER.acquire().await;

        // 3) Ask every provider in parallel (`crate::web_search`). Each is free
        //    and key-less; a dead one is named in `note` and is never fatal.
        let timeout = Duration::from_millis(state.config.web.search_timeout_ms.max(200));
        let cooldown = Duration::from_secs(state.config.web.search_cooldown_secs);
        let mut endpoints = state.config.web.endpoints.clone();
        // The historical single-provider override stays honoured.
        if let Some(base) = state.config.web.ddg_api.clone() {
            endpoints.entry("duckduckgo".to_string()).or_insert(base);
        }
        let skip = providers_in_cooldown();

        let run =
            crate::web_search::search_all(&HTTP, &endpoints, &query, limit, timeout, &skip).await;

        // Cooldown bookkeeping: a provider that contributed clears its own
        // cooldown; a provider that failed earns one.
        for name in &run.contributors {
            note_provider_success(name);
        }
        let mut note = String::new();
        for (name, err) in &run.failures {
            if cooldown.as_secs() > 0 {
                note_provider_failure(name, cooldown);
            }
            note.push_str(&format!("{name}: {err}; "));
        }
        if !run.skipped.is_empty() {
            note.push_str(&format!("skipped (cooldown): {}; ", run.skipped.join(", ")));
        }
        let note = note.trim_end_matches("; ").to_string();

        if run.hits.is_empty() {
            let reason = if note.is_empty() {
                "all providers answered, none had results".to_string()
            } else {
                note
            };
            return tool_error(format!("web_search: no results ({reason})"));
        }

        answer(&state, &query, limit, run.hits, &run.contributors, &note)
    }
}

/// Cache and shape a successful answer. Descriptions are capped so one result
/// cannot flood the context. `contributors` names every provider that answered;
/// `provider` is that single name when only one contributed, else `fused`.
fn answer(
    state: &Arc<AppState>,
    query: &str,
    limit: usize,
    hits: Vec<Value>,
    contributors: &[&str],
    note: &str,
) -> String {
    let hits: Vec<Value> = hits.into_iter().map(cap_hit).collect();
    let provider = match contributors {
        [only] => *only,
        _ if contributors.len() > 1 => "fused",
        _ => "none",
    };
    let mut obj = json!({
        "data": { "web": hits },
        "provider": provider,
        "providers": contributors,
    });
    if !note.is_empty() {
        obj["note"] = json!(note);
    }
    let out = obj.to_string();
    cache_put(state, query, limit, &out);
    out
}

// ─── Per-URL content cache (a page is fetched once) ───────────────────────
//
// The 80/20 of a local index without a search index: store the *rendered* page
// (the Markdown + metadata we would return anyway) under its canonical URL, so a
// second look at the same URL costs no network at all. Keyed by the parameters
// that shape the answer (`char_limit`, `passages`) — the same rule as the search
// cache, where `limit` is part of the key.

fn url_cache_key(url: &str, char_limit: usize, passages: bool) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    crate::web_search::canonical_url(url).hash(&mut hasher);
    char_limit.hash(&mut hasher);
    passages.hash(&mut hasher);
    format!("url_cache:{:x}", hasher.finish())
}

fn url_cache_get(
    state: &Arc<AppState>,
    url: &str,
    char_limit: usize,
    passages: bool,
) -> Option<Value> {
    let key = url_cache_key(url, char_limit, passages);
    let (val, saved_at) = state.brain.kv_cache_get(&key)?;
    if (now_secs() as i64 - saved_at) >= cache_ttl(state) as i64 {
        return None;
    }
    let mut v: Value = serde_json::from_str(&val).ok()?;
    v["cached"] = json!(true);
    Some(v)
}

fn url_cache_put(
    state: &Arc<AppState>,
    url: &str,
    char_limit: usize,
    passages: bool,
    value: &Value,
) {
    let key = url_cache_key(url, char_limit, passages);
    state.brain.kv_cache_put(&key, &value.to_string());
}

pub struct WebExtract;

#[async_trait]
impl AsyncTool for WebExtract {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "web_extract",
            description: "Extract the main article (Readability) as Markdown from web page URLs.",
            emoji: "📄",
            toolset: "web",
            is_async: true,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &WEB_EXTRACT_SCHEMA
    }

    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let urls = match args.get("urls") {
            Some(Value::Array(arr)) => {
                let urls: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).take(5).collect();
                if urls.is_empty() {
                    return tool_error("urls must be a non-empty array of URL strings");
                }
                urls
            }
            _ => return tool_error("urls is required and must be an array of URL strings"),
        };

        let char_limit = args
            .get("char_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(15000) as usize;
        let want_passages = args
            .get("passages")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // 1) Per-URL cache first: a page we already rendered costs no network.
        //    The lookups are cheap (SQLite), so they run before any spawn.
        let mut slots: Vec<Option<Value>> = vec![None; urls.len()];
        let mut to_fetch: Vec<usize> = Vec::new();
        for (idx, url) in urls.iter().enumerate() {
            match url_cache_get(&state, url, char_limit, want_passages) {
                Some(cached) => slots[idx] = Some(cached),
                None => to_fetch.push(idx),
            }
        }

        // 2) Fetch the misses in parallel instead of one-after-another: five
        //    pages used to cost the sum of their latencies (measured 0.8-3.6 s
        //    per URL, so up to ~15 s). Spawned tasks keep the caller's order.
        let client = HTTP.clone();
        let parallelism = state.config.web.extract_parallel.max(1);
        let mut pending: Vec<(usize, tokio::task::JoinHandle<Value>)> = Vec::new();
        let mut queue = to_fetch.iter().copied().peekable();
        let mut done = 0usize;
        loop {
            while pending.len() < parallelism {
                match queue.next() {
                    Some(idx) => {
                        let client = client.clone();
                        let url = urls[idx].to_string();
                        pending.push((
                            idx,
                            tokio::spawn(async move {
                                extract_single_page(&client, &url, char_limit, want_passages).await
                            }),
                        ));
                    }
                    None => break,
                }
            }
            if pending.is_empty() {
                break;
            }
            let (idx, handle) = pending.remove(0);
            let value = handle.await.unwrap_or_else(|e| {
                json!({
                    "url": urls[idx],
                    "title": "",
                    "content": "",
                    "error": format!("extract task failed: {e}")
                })
            });
            // Cache real content only — never an error page.
            let failed = value
                .get("error")
                .and_then(|e| e.as_str())
                .map(|e| !e.trim().is_empty())
                .unwrap_or(false);
            if !failed {
                url_cache_put(&state, urls[idx], char_limit, want_passages, &value);
            }
            slots[idx] = Some(value);
            done += 1;
            if done == to_fetch.len() {
                break;
            }
        }

        // 3) Assemble in the caller's order (cache hits and fresh fetches alike).
        let results: Vec<Value> = slots.into_iter().flatten().collect();
        json!({ "results": results }).to_string()
    }
}

/// Largest page body we buffer before giving up (5 MiB). Without this a runaway
/// response was read fully and pushed through the HTML parser.
const MAX_PAGE_BYTES: usize = 5 * 1024 * 1024;

async fn extract_single_page(
    client: &reqwest::Client,
    url: &str,
    char_limit: usize,
    want_passages: bool,
) -> Value {
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            return json!({
                "url": url,
                "title": "",
                "content": "",
                "error": format!("HTTP request failed: {e}")
            })
        }
    };

    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    // A 4xx/5xx body is an error page, not content.
    if !status.is_success() {
        return json!({
            "url": url,
            "title": "",
            "content": "",
            "error": format!("HTTP {} ({})", status.as_u16(), content_type)
        });
    }

    // Reject oversized responses before buffering them.
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_PAGE_BYTES {
            return json!({
                "url": url,
                "title": "",
                "content": "",
                "error": format!("page too large: {len} bytes (limit {MAX_PAGE_BYTES})")
            });
        }
    }

    // Handle PDFs — the binary body would be noise for the HTML parser.
    if content_type.contains("pdf") {
        return json!({
            "url": url,
            "title": format!("PDF: {url}"),
            "content": "[PDF content - use a PDF reader for full text]",
            "error": ""
        });
    }

    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            return json!({
                "url": url,
                "title": "",
                "content": "",
                "error": format!("Failed to read response body: {e}")
            })
        }
    };
    // Defensive second check for chunked responses without a Content-Length.
    if body.len() > MAX_PAGE_BYTES {
        return json!({
            "url": url,
            "title": "",
            "content": "",
            "error": format!(
                "page too large: {} bytes (limit {MAX_PAGE_BYTES})",
                body.len()
            )
        });
    }

    render_page(&body, url, char_limit, want_passages)
}

// ─── Article extraction (Readability → Markdown) ──────────────────────────
//
// Run the page through Mozilla's Readability algorithm to keep the ARTICLE,
// then emit Markdown. Pushing the whole document (nav, cookie banner, footer)
// through a text converter made boilerplate dominate short pages and turned the
// "title" into the first line of the dump.

/// Below this many characters a "readable" extraction is treated as a stub
/// (usually a navigational shell) and the whole-page fallback is used instead.
const MIN_ARTICLE_CHARS: usize = 200;

/// Metadata carried from a Readability `Article` into the tool result.
#[derive(Default)]
struct PageMeta {
    byline: Option<String>,
    site_name: Option<String>,
    published_time: Option<String>,
    excerpt: Option<String>,
}

/// Extract the article (Readability) or fall back to whole-page Markdown.
/// `readability: true` in the result marks the extracted-article path.
fn render_page(html: &str, url: &str, char_limit: usize, want_passages: bool) -> Value {
    // `is_probably_readable` must run before `parse` — it rewrites the document.
    let cfg = dom_smoothie::Config {
        text_mode: dom_smoothie::TextMode::Markdown,
        max_elements_to_parse: 50_000,
        ..Default::default()
    };
    if let Ok(mut readability) = dom_smoothie::Readability::new(html, Some(url), Some(cfg)) {
        if readability.is_probably_readable() {
            if let Ok(article) = readability.parse() {
                let text = article.text_content.to_string();
                if text.trim().chars().count() >= MIN_ARTICLE_CHARS {
                    let title = if article.title.trim().is_empty() {
                        None
                    } else {
                        Some(article.title.trim().to_string())
                    };
                    let meta = PageMeta {
                        byline: non_empty(article.byline),
                        site_name: non_empty(article.site_name),
                        published_time: non_empty(article.published_time),
                        excerpt: non_empty(article.excerpt),
                    };
                    return finish_page(url, title, &text, char_limit, want_passages, true, meta);
                }
            }
        }
    }

    // Fallback: whole-page Markdown (nav/footer included — flagged honestly).
    let markdown = htmd::convert(html).unwrap_or_else(|_| nanohtml2text::html2text(html));
    finish_page(
        url,
        None,
        &markdown,
        char_limit,
        want_passages,
        false,
        PageMeta::default(),
    )
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Shape one page result: capped Markdown + metadata (+ optional passages).
fn finish_page(
    url: &str,
    title: Option<String>,
    text: &str,
    char_limit: usize,
    want_passages: bool,
    readability: bool,
    meta: PageMeta,
) -> Value {
    let (content, truncated) = truncate_middle(text, char_limit);
    let mut v = json!({
        "url": url,
        "title": title.unwrap_or_else(|| first_heading(text)),
        "content": content.trim(),
        "truncated": truncated,
        "char_count": text.chars().count(),
        "readability": readability,
    });
    if let Some(b) = meta.byline {
        v["byline"] = json!(b);
    }
    if let Some(s) = meta.site_name {
        v["site_name"] = json!(s);
    }
    if let Some(p) = meta.published_time {
        v["published_time"] = json!(p);
    }
    if let Some(e) = meta.excerpt {
        v["excerpt"] = json!(e);
    }
    if want_passages {
        v["passages"] = json!(passages_of(text, 1000, 40));
    }
    v
}

/// Split Markdown into paragraph passages (for citations and passage-level use).
fn passages_of(text: &str, max_each: usize, max_n: usize) -> Vec<Value> {
    text.split("\n\n")
        .map(str::trim)
        .filter(|p| p.chars().count() >= 20)
        .take(max_n)
        .enumerate()
        .map(|(i, p)| {
            let text = if p.chars().count() > max_each {
                let cut: String = p.chars().take(max_each).collect();
                format!("{cut}…")
            } else {
                p.to_string()
            };
            json!({ "index": i, "text": text })
        })
        .collect()
}

/// The first Markdown heading/line — the fallback title when Readability found none.
fn first_heading(md: &str) -> String {
    md.lines()
        .next()
        .unwrap_or("")
        .trim_start_matches("# ")
        .trim_start_matches('#')
        .trim()
        .to_string()
}

// ─── Utils ────────────────────────────────────────────────────────────────

/// Keep the head and tail of `s` up to `limit` CHARACTERS (never splits a
/// multi-byte character — byte slicing panics on any non-ASCII page).
fn truncate_middle(s: &str, limit: usize) -> (String, bool) {
    let total = s.chars().count();
    if total <= limit {
        return (s.to_string(), false);
    }
    let half = limit / 2;
    let head: String = s.chars().take(half).collect();
    let tail: String = s.chars().skip(total - half).collect();
    (
        format!("{head}\n\n... [Content truncated at {limit} chars] ...\n\n{tail}"),
        true,
    )
}

/// Cap a hit's description so one result cannot flood the context.
const MAX_SNIPPET_CHARS: usize = 400;
fn snippet(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > MAX_SNIPPET_CHARS {
        let mut out: String = s.chars().take(MAX_SNIPPET_CHARS).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

fn cap_hit(mut hit: Value) -> Value {
    if let Some(desc) = hit.get("description").and_then(|v| v.as_str()) {
        let capped = snippet(desc);
        if capped != desc {
            hit["description"] = Value::String(capped);
        }
    }
    hit
}

lazy_static::lazy_static! {
    static ref WEB_SEARCH_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Search query" },
            "limit": { "type": "integer", "description": "Max results (default 5)", "minimum": 1, "maximum": 100 }
        },
        "required": ["query"]
    }));

    static ref WEB_EXTRACT_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "urls": { "type": "array", "items": { "type": "string" }, "description": "URLs (max 5)" },
            "char_limit": { "type": "integer", "description": "Chars per page (default 15000)", "minimum": 2000, "maximum": 100000 },
            "passages": { "type": "boolean", "description": "Also return the article split into passages (default false)" }
        },
        "required": ["urls"]
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AsyncTool;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    lazy_static::lazy_static! {
        /// The cooldown map and `NO_PROXY`-style env are process-wide.
        static ref ENV_LOCK: Mutex<()> = Mutex::new(());
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Serves one canned body per request, after an optional delay. A thread per
    /// connection so the parallelism test can actually overlap.
    fn json_server(body: &'static str, delay_ms: u64) -> (u16, std::sync::Arc<Mutex<usize>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(Mutex::new(0usize));
        let hits_thread = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let hits = hits_thread.clone();
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 4096];
                    let _ = stream.read(&mut buffer);
                    *hits.lock().unwrap() += 1;
                    if delay_ms > 0 {
                        std::thread::sleep(Duration::from_millis(delay_ms));
                    }
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                });
            }
        });
        (port, hits)
    }

    /// Answers every request with a fixed HTTP status (to test error handling).
    fn status_server(code: u16, body: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 4096];
                    let _ = stream.read(&mut buffer);
                    let response = format!(
                        "HTTP/1.1 {code} Status\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                });
            }
        });
        port
    }

    /// A DuckDuckGo-Lite-shaped page (one result) for the built-in engine.
    const DDG_HTML: &str = r#"<a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&rut=x" class='result-link'>Rust Programming Language</a><td class='result-snippet'><b>Rust</b> is a fast language.</td>"#;
    /// No `result-link` → the engine answers with 0 hits.
    const EMPTY_HTML: &str = "<html><body>no results</body></html>";
    /// A Wikipedia-API-shaped body (one result) for the provider list.
    const WIKI_BODY: &str = r#"{"query":{"search":[{"title":"Wiki hit","snippet":"from <span class=\"searchmatch\">wikipedia</span>"}]}}"#;
    /// A crates.io-shaped body (one result): a different provider, same hit shape.
    const CRATES_BODY: &str = r#"{"crates":[{"name":"example-crate","max_version":"1.2.3","description":"from crates.io"}]}"#;

    /// Point every provider at one base URL (a local server), so no test reaches
    /// the network. Only the provider whose parser understands the body answers.
    fn endpoints_all_to(base: &str) -> std::collections::BTreeMap<String, String> {
        crate::web_search::PROVIDERS
            .iter()
            .map(|(name, _, _)| (name.to_string(), base.to_string()))
            .collect()
    }

    /// Build an `AppState` with explicit per-provider endpoints.
    fn make_state_with(
        endpoints: std::collections::BTreeMap<String, String>,
        timeout_ms: u64,
        cooldown_s: u64,
    ) -> Arc<AppState> {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.data_dir = dir.path().to_path_buf();
        config.web.search_timeout_ms = timeout_ms;
        config.web.search_cooldown_secs = cooldown_s;
        config.web.endpoints = endpoints;
        reset_all_cooldowns();
        std::mem::forget(dir); // keep the temp dir alive for the state's lifetime
        Arc::new(AppState {
            brain: Arc::new(
                crate::brain::db::BrainDb::open(&config.data_dir.join("brain.db"), 384).unwrap(),
            ),
            embedder: tokio::sync::Mutex::new(None),
            config,
            session_id: crate::AppState::new_session_id(),
            tools: Arc::new(crate::tools::Registry::build_default()),
            ui_tx: Mutex::new(None),
            files_read: Mutex::new(std::collections::HashSet::new()),
            cancel: std::sync::atomic::AtomicBool::new(false),
            router: std::sync::Mutex::new(None),
        })
    }

    /// Every provider answers from the same local server.
    fn make_state(base: String, timeout_ms: u64, cooldown_s: u64) -> Arc<AppState> {
        make_state_with(endpoints_all_to(&base), timeout_ms, cooldown_s)
    }

    fn provider_of(out: &str) -> String {
        serde_json::from_str::<Value>(out)
            .ok()
            .and_then(|v| v.get("provider").and_then(|p| p.as_str()).map(String::from))
            .unwrap_or_default()
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn duckduckgo_answers_and_unwraps_urls() {
        let _guard = env_lock();
        let (port, _) = json_server(DDG_HTML, 0);
        let state = make_state(format!("http://127.0.0.1:{port}/lite/"), 2000, 900);

        let out = WebSearch
            .call_async(json!({"query": "rust", "limit": 5}), state.clone())
            .await;
        assert_eq!(provider_of(&out), "duckduckgo", "{out}");
        assert!(
            out.contains("https://rust-lang.org/"),
            "the DDG redirect must be unwrapped: {out}"
        );
    }

    /// The same query+limit must come from the cache. Every provider in the list
    /// is asked once, so the assertion counts *additional* requests, not `1`.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn search_results_are_cached() {
        let _guard = env_lock();
        let (port, hits) = json_server(DDG_HTML, 0);
        let state = make_state(format!("http://127.0.0.1:{port}/"), 2000, 900);

        let _ = WebSearch
            .call_async(json!({"query": "rust", "limit": 5}), state.clone())
            .await;
        let after_first = *hits.lock().unwrap();
        assert!(after_first > 0, "the providers were never asked");

        let _ = WebSearch
            .call_async(json!({"query": "rust", "limit": 5}), state.clone())
            .await;
        assert_eq!(
            *hits.lock().unwrap(),
            after_first,
            "the same query+limit must come from the cache"
        );
    }

    /// Two providers with different bodies: both survive the merge, the answer
    /// names them, and every hit carries its own provenance.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn two_providers_are_merged_and_reported() {
        let _guard = env_lock();
        let (wiki_port, _) = json_server(WIKI_BODY, 0);
        let (crates_port, _) = json_server(CRATES_BODY, 0);
        let mut endpoints = endpoints_all_to(&format!("http://127.0.0.1:{wiki_port}/"));
        endpoints.insert(
            "crates-io".to_string(),
            format!("http://127.0.0.1:{crates_port}/"),
        );
        let state = make_state_with(endpoints, 2000, 0);

        let out = WebSearch
            .call_async(json!({"query": "rust", "limit": 10}), state.clone())
            .await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["providers"], json!(["crates-io", "wikipedia"]), "{out}");
        assert_eq!(v["provider"], json!("fused"), "{out}");

        let web = v["data"]["web"].as_array().unwrap();
        assert_eq!(web.len(), 2, "one hit per provider: {out}");
        let by_url = |needle: &str| {
            web.iter()
                .find(|h| h["url"].as_str().unwrap_or("").contains(needle))
                .unwrap_or_else(|| panic!("no hit for {needle}: {out}"))
        };
        assert_eq!(by_url("crates.io")["sources"], json!(["crates-io"]));
        assert_eq!(by_url("wikipedia")["sources"], json!(["wikipedia"]));
        // The Wikipedia snippet's markup is stripped on the way out.
        assert!(
            !by_url("wikipedia")["description"]
                .as_str()
                .unwrap_or("")
                .contains('<'),
            "{out}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_failing_engine_is_reported() {
        let _guard = env_lock();
        let port = status_server(500, "nope");
        let state = make_state(format!("http://127.0.0.1:{port}/lite/"), 1000, 0);

        let out = WebSearch
            .call_async(json!({"query": "x", "limit": 5}), state.clone())
            .await;
        assert!(out.contains("error"), "a 5xx must be reported: {out}");
    }

    /// A representative article page: nav + cookie banner + article + footer.
    const ARTICLE_PAGE: &str = r#"<!doctype html><html><head>
<title>Die Zukunft von Rust - Beispiel</title>
<meta property="og:site_name" content="Beispiel">
<meta property="article:published_time" content="2026-01-02T03:04:05Z">
</head><body>
<header><nav><a href="/">Startseite</a> | <a href="/about">Ueber uns</a></nav></header>
<div id="cookie-banner">Wir verwenden Cookies, um die Navigation zu verbessern. Bitte akzeptieren Sie die Cookies.</div>
<main><article>
<h1>Die Zukunft von Rust</h1>
<p>Rust ist eine Systemprogrammiersprache, die Speichersicherheit ohne Garbage Collector garantiert. Sie wird zunehmend fuer Betriebssysteme, Netzwerkdienste und eingebettete Systeme eingesetzt und verbindet die Kontrolle von C mit einer modernen Toolchain.</p>
<p>Die Sprache waechst schnell: Die Werkzeugkette cargo vereinfacht das Bauen und Testen, und das Oekosystem deckt Web-Frameworks, Datenbanken und KI-Bibliotheken ab. Viele Unternehmen ersetzen sicherheitskritische Komponenten durch Rust.</p>
<p>Experten erwarten, dass Rust in den naechsten Jahren auch in der Lehre und in regulierten Branchen an Bedeutung gewinnt, weil die strengen Compiler-Regeln ganze Fehlerklassen ausschliessen.</p>
</article></main>
<footer>Impressum - Datenschutz - Kontakt</footer>
</body></html>"#;

    /// Readability keeps the article and drops boilerplate, where the old
    /// whole-page converter kept the nav and cookie banner.
    #[test]
    fn readability_extraction_beats_whole_page_text() {
        let url = "https://example.org/rust";
        let whole_page = nanohtml2text::html2text(ARTICLE_PAGE);
        let v = render_page(ARTICLE_PAGE, url, 15000, true);
        let content = v["content"].as_str().unwrap_or("");

        assert_eq!(v["readability"], json!(true), "article path expected: {v}");
        assert!(
            content.contains("Systemprogrammiersprache"),
            "article body missing: {content}"
        );
        assert!(
            !content.contains("Cookies")
                && !content.contains("Impressum")
                && !content.contains("Startseite"),
            "boilerplate leaked into the article: {content}"
        );
        assert!(
            whole_page.contains("Cookies") && whole_page.contains("Impressum"),
            "fixture should expose the whole-page boilerplate: {whole_page}"
        );
        assert!(
            v["title"]
                .as_str()
                .unwrap_or("")
                .contains("Zukunft von Rust"),
            "title not from the article metadata: {v}"
        );
        assert_eq!(v["site_name"], json!("Beispiel"), "{v}");
        assert_eq!(v["published_time"], json!("2026-01-02T03:04:05Z"), "{v}");
        assert!(
            v["passages"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            "passages expected: {v}"
        );
        let without = render_page(ARTICLE_PAGE, url, 15000, false);
        assert!(without.get("passages").is_none(), "{without}");

        let ratio = content.chars().count() as f64 / whole_page.chars().count().max(1) as f64;
        eprintln!(
            "web-extract eval: whole-page {} chars (boilerplate kept) -> readability {} chars (ratio {:.0}%)",
            whole_page.chars().count(),
            content.chars().count(),
            ratio * 100.0
        );
    }

    /// A non-readable / tiny page falls back to whole-page Markdown and says so.
    #[test]
    fn non_readable_page_falls_back_and_flags_it() {
        let v = render_page(
            "<html><body><p>kurz</p></body></html>",
            "https://x/y",
            15000,
            false,
        );
        assert_eq!(v["readability"], json!(false), "{v}");
        assert!(
            v["content"].as_str().unwrap_or("").contains("kurz"),
            "fallback must still return the text: {v}"
        );
    }

    /// The middle-truncation must slice by CHARACTERS: byte slicing panicked on
    /// any non-ASCII page ("byte index is not a char boundary").
    #[test]
    fn truncate_middle_is_char_safe_on_non_ascii() {
        let s = "ä".repeat(300);
        let (out, truncated) = truncate_middle(&s, 100);
        assert!(truncated);
        assert!(
            out.starts_with('ä') && out.ends_with('ä'),
            "garbled: {out:?}"
        );
        assert!(out.contains("truncated at 100 chars"), "{out}");
        let cjk = "漢字".repeat(50);
        let (out2, trunc2) = truncate_middle(&cjk, 40);
        assert!(trunc2 && out2.ends_with('字'), "garbled: {out2:?}");
        let (same, trunc) = truncate_middle("München", 100);
        assert_eq!(same, "München");
        assert!(!trunc);
    }

    /// A description is capped so one result cannot flood the context.
    #[test]
    fn descriptions_are_capped() {
        let hit = cap_hit(json!({"title": "t", "url": "u", "description": "x".repeat(5000)}));
        let d = hit["description"].as_str().unwrap();
        assert!(
            d.chars().count() <= MAX_SNIPPET_CHARS + 1,
            "description not capped ({} chars)",
            d.chars().count()
        );
        assert!(d.ends_with('…'), "capped text should be marked: {d:?}");
        let short = cap_hit(json!({"description": "kurz"}));
        assert_eq!(short["description"], json!("kurz"));
    }

    /// A 4xx/5xx body is an error page, not content.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn extract_reports_http_error_status() {
        let _guard = env_lock();
        let port = status_server(404, "<html><body>nicht da</body></html>");
        let (ddg_port, _) = json_server(EMPTY_HTML, 0);
        let state = make_state(format!("http://127.0.0.1:{ddg_port}/lite/"), 2000, 0);
        let out = WebExtract
            .call_async(
                json!({"urls": [format!("http://127.0.0.1:{port}/missing")]}),
                state.clone(),
            )
            .await;
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let entry = &parsed["results"][0];
        assert!(
            entry["error"].as_str().unwrap_or("").contains("404"),
            "a 404 must be reported as an error: {out}"
        );
        assert!(
            entry["content"].as_str().unwrap_or("").is_empty(),
            "an error page must not be returned as content: {out}"
        );
    }

    /// Three slow pages in one call: parallel fetch must take about as long as
    /// the slowest page, not the sum.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn extract_fetches_in_parallel() {
        let _guard = env_lock();
        let delay = 300;
        let (port, hits) = json_server(r#"{"ok":true}"#, delay);
        let (ddg_port, _) = json_server(EMPTY_HTML, 0);
        let state = make_state(format!("http://127.0.0.1:{ddg_port}/lite/"), 2000, 0);
        let urls: Vec<String> = (0..3)
            .map(|i| format!("http://127.0.0.1:{port}/p{i}"))
            .collect();

        let started = std::time::Instant::now();
        let out = WebExtract
            .call_async(json!({"urls": urls, "char_limit": 2000}), state.clone())
            .await;
        let elapsed = started.elapsed();

        let parsed: Value = serde_json::from_str(&out).unwrap();
        let results = parsed["results"].as_array().unwrap();
        assert_eq!(results.len(), 3, "{out}");
        assert_eq!(*hits.lock().unwrap(), 3);
        assert!(
            elapsed < Duration::from_millis(750),
            "3 pages at {delay} ms took {elapsed:?} — not parallel"
        );
        for (i, entry) in results.iter().enumerate() {
            assert!(
                entry["url"]
                    .as_str()
                    .unwrap_or("")
                    .ends_with(&format!("/p{i}")),
                "order changed: {out}"
            );
        }
    }

    /// A page is fetched once: the second `web_extract` for the same URL serves
    /// the stored render (no network), and says so. A different `char_limit` is a
    /// different cache entry.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_url_is_extracted_once_then_served_from_the_cache() {
        let _guard = env_lock();
        let (port, hits) = json_server(ARTICLE_PAGE, 0);
        let (ddg_port, _) = json_server(EMPTY_HTML, 0);
        let state = make_state(format!("http://127.0.0.1:{ddg_port}/"), 2000, 0);
        let url = format!("http://127.0.0.1:{port}/article");

        let first = WebExtract
            .call_async(
                json!({"urls": [url.clone()], "char_limit": 4000}),
                state.clone(),
            )
            .await;
        let f: Value = serde_json::from_str(&first).unwrap();
        assert!(
            f["results"][0]["content"]
                .as_str()
                .unwrap_or("")
                .contains("Systemprogrammiersprache"),
            "the article must be extracted: {first}"
        );
        assert!(f["results"][0].get("cached").is_none(), "{first}");
        assert_eq!(*hits.lock().unwrap(), 1);

        let second = WebExtract
            .call_async(
                json!({"urls": [url.clone()], "char_limit": 4000}),
                state.clone(),
            )
            .await;
        assert_eq!(
            *hits.lock().unwrap(),
            1,
            "the second call fetched again: {second}"
        );
        let s: Value = serde_json::from_str(&second).unwrap();
        assert_eq!(s["results"][0]["cached"], json!(true), "{second}");
        assert_eq!(
            s["results"][0]["content"], f["results"][0]["content"],
            "{second}"
        );

        // Same URL, different shaping parameters → a different cache entry.
        let _ = WebExtract
            .call_async(json!({"urls": [url], "char_limit": 8000}), state.clone())
            .await;
        assert_eq!(
            *hits.lock().unwrap(),
            2,
            "a different char_limit is a different cache entry"
        );
    }

    /// Live: the second `web_extract` of the same URL must cost no network.
    /// `cargo test -p rijo-core -- --ignored --nocapture live_extract_cache`
    #[tokio::test]
    #[ignore = "live network: run with --ignored --nocapture"]
    async fn live_extract_cache_measurement() {
        let _guard = env_lock();
        let state = make_state_with(Default::default(), 8000, 0);
        let url = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
        for pass in 1..=2 {
            let started = std::time::Instant::now();
            let out = WebExtract
                .call_async(json!({"urls": [url], "char_limit": 20000}), state.clone())
                .await;
            let ms = started.elapsed().as_millis();
            let v: Value = serde_json::from_str(&out).unwrap_or_else(|_| json!({}));
            let r = &v["results"][0];
            eprintln!(
                "live web_extract pass {pass}: {ms} ms | cached={} | readability={} | {} chars",
                r.get("cached").unwrap_or(&json!(false)),
                r["readability"],
                r["char_count"]
            );
        }
    }

    /// Live end-to-end measurement against the **real** providers. Offline by
    /// default: `cargo test -p rijo-core -- --ignored --nocapture live_search`.
    #[tokio::test]
    #[ignore = "live network: run with --ignored --nocapture"]
    async fn live_search_measurement() {
        let _guard = env_lock();
        // An empty override map = the real endpoints.
        let state = make_state_with(Default::default(), 8000, 0);
        for query in [
            "rust async http client",
            "recursive rank fusion",
            "tokio task spawn blocking",
        ] {
            let started = std::time::Instant::now();
            let out = WebSearch
                .call_async(json!({"query": query, "limit": 5}), state.clone())
                .await;
            let ms = started.elapsed().as_millis();
            let v: Value = serde_json::from_str(&out).unwrap_or_else(|_| json!({}));
            let urls: Vec<String> = v["data"]["web"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|h| h["url"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            eprintln!(
                "live web_search [{query}]: {ms} ms | provider={} | providers={} | {} hits\n    first: {}\n    note:  {}",
                v["provider"],
                v["providers"],
                urls.len(),
                urls.first().cloned().unwrap_or_default(),
                v["note"].as_str().unwrap_or("")
            );
            assert!(!urls.is_empty(), "no hits for {query}: {out}");
        }
    }

    /// Live measurement on a real page. Offline by default (network is flaky in
    /// CI): `cargo test -p rijo-core -- --ignored --nocapture live_article`.
    #[tokio::test]
    #[ignore = "live network: run with --ignored --nocapture"]
    async fn live_article_extraction_measurement() {
        for url in [
            "https://en.wikipedia.org/wiki/Rust_(programming_language)",
            "https://blog.rust-lang.org/",
        ] {
            let html = match HTTP.get(url).send().await {
                Ok(r) => match r.text().await {
                    Ok(t) => t,
                    Err(_) => continue,
                },
                Err(_) => continue, // offline: skip silently
            };
            let whole = nanohtml2text::html2text(&html);
            let v = render_page(&html, url, 20000, false);
            let content = v["content"].as_str().unwrap_or("");
            eprintln!(
                "live [{}]: page {} chars -> readability {} chars (readability={}, title={:?})",
                url,
                whole.chars().count(),
                content.chars().count(),
                v["readability"],
                v["title"].as_str().unwrap_or("")
            );
            assert!(!content.is_empty(), "no content for {url}");
        }
    }
}
