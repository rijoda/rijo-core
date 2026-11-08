//! arXiv research tool — search and analyse papers over public APIs, no key.
//!
//! Two sources, one tool:
//! - **arXiv** (`export.arxiv.org/api/query`, Atom XML) for search and metadata;
//! - **Semantic Scholar** (JSON) for citation counts, references and citations.
//!
//! Speed and manners: one pooled HTTP client, a 24 h cache in the same `brain`
//! key-value store `web_search` uses, and the documented rate limits respected —
//! arXiv allows one request per three seconds and answers a hammering client with
//! a temporary block. A descriptive User-Agent is sent because arXiv asks for one.
//!
//! Five actions: `search`, `get`, `citations`, `references`, `cited_by`.

use crate::tools::{tool_error, AsyncTool, JsonSchema, ToolMeta, ToolResult};
use crate::AppState;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

const ARXIV_API: &str = "https://export.arxiv.org/api/query";
const S2_PAPER: &str = "https://api.semanticscholar.org/graph/v1/paper";
/// arXiv asks for a descriptive agent; a bare reqwest default is what gets rate
/// limited first.
const UA: &str = concat!(
    "rijo-core/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/rijoda/rijo-core)"
);
const DEFAULT_CACHE_TTL: u64 = 24 * 60 * 60;

lazy_static::lazy_static! {
    static ref HTTP: reqwest::Client = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .unwrap_or_default();

    /// Last arXiv request. The API wants ≥3 s between calls; a burst earns a
    /// temporary block, so the tool queues instead of hammering.
    static ref LAST_ARXIV: Mutex<Option<Instant>> = Mutex::new(None);
    /// Last Semantic Scholar request (1/s without an API key).
    static ref LAST_S2: Mutex<Option<Instant>> = Mutex::new(None);
}

/// Wait until at least `min_gap` has passed since the last call on `slot`, then
/// mark now. A single lock covers the check and the stamp, so parallel callers
/// queue instead of all firing at once.
async fn throttle(slot: &Mutex<Option<Instant>>, min_gap: Duration) {
    loop {
        let wait = {
            let guard = slot.lock().unwrap_or_else(|p| p.into_inner());
            match *guard {
                Some(last) => min_gap.checked_sub(last.elapsed()),
                None => None,
            }
        };
        match wait {
            None | Some(Duration::ZERO) => {
                *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
                return;
            }
            Some(d) => tokio::time::sleep(d).await,
        }
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn cache_key(action: &str, params: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    action.hash(&mut h);
    params.hash(&mut h);
    format!("arxiv:{:x}", h.finish())
}

fn cache_ttl(state: &AppState) -> u64 {
    match state.config.web.cache_ttl_secs {
        0 => DEFAULT_CACHE_TTL,
        n => n,
    }
}

fn cache_get(state: &AppState, key: &str) -> Option<String> {
    let (val, saved_at) = state.brain.kv_cache_get(key)?;
    ((now_secs() - saved_at) < cache_ttl(state) as i64).then_some(val)
}

fn cache_put(state: &AppState, key: &str, value: &str) {
    state.brain.kv_cache_put(key, value);
}

// ─── Query building ──────────────────────────────────────────────────────

/// Turn a human query into arXiv query syntax. A query that already names a
/// field (`au:`, `ti:`, `cat:`, …) is passed through unchanged; anything else is
/// prefixed with `all:` so a plain phrase still searches every field.
fn to_search_query(query: &str) -> String {
    let q = query.trim();
    let has_field = [
        "all:", "ti:", "au:", "abs:", "cat:", "co:", "rn:", "jr:", "id:",
    ]
    .iter()
    .any(|f| q.contains(f));
    let body = if has_field {
        q.to_string()
    } else {
        format!("all:{q}")
    };
    encode_query(&body)
}

/// Spaces become `+` (the API's own convention). Only the characters that would
/// break the URL or inject a parameter are percent-encoded — `"`, `:` `,` `(` and
/// `+` are part of arXiv's query syntax and must survive.
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ' ' | '\t' | '\n' => out.push('+'),
            '&' | '#' | '%' | '?' | '<' | '>' => {
                out.push_str(&format!("%{:02X}", c as u32));
            }
            _ => out.push(c),
        }
    }
    out
}

/// `2402.03300v3` → `2402.03300` (Semantic Scholar keys without the version).
///
/// Old-style ids contain a slash (`hep-th/0601001`) and must keep it, so only a
/// full `/abs/` URL is reduced — and the `vN` suffix is only removed when a digit
/// precedes the `v` (never a `v` inside an archive name).
fn strip_version(id: &str) -> String {
    let id = id.rsplit("/abs/").next().unwrap_or(id);
    match id.rfind('v') {
        Some(i)
            if i > 0
                && id.as_bytes()[i - 1].is_ascii_digit()
                && id[i + 1..].chars().all(|c| c.is_ascii_digit()) =>
        {
            id[..i].to_string()
        }
        _ => id.to_string(),
    }
}

// ─── Fetching ────────────────────────────────────────────────────────────

async fn arxiv_get(url: &str) -> Result<String, String> {
    throttle(&LAST_ARXIV, Duration::from_secs(3)).await;
    let resp = HTTP.get(url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "arXiv HTTP {}: {}",
            status.as_u16(),
            body.chars().take(160).collect::<String>()
        ));
    }
    Ok(body)
}

async fn s2_get(path: &str) -> Result<Value, String> {
    throttle(&LAST_S2, Duration::from_secs(1)).await;
    let resp = HTTP
        .get(format!("{S2_PAPER}{path}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "Semantic Scholar HTTP {}: {}",
            status.as_u16(),
            body.chars().take(160).collect::<String>()
        ));
    }
    serde_json::from_str(&body).map_err(|e| format!("Semantic Scholar json: {e}"))
}

// ─── Atom parsing ────────────────────────────────────────────────────────

#[derive(Default)]
struct Entry {
    id: String,
    title: String,
    summary: String,
    published: String,
    updated: String,
    authors: Vec<String>,
    categories: Vec<String>,
    primary: String,
    comment: String,
    journal_ref: String,
    doi: String,
    pdf: String,
}

impl Entry {
    fn into_json(self) -> Value {
        // The feed's id is the abs URL; keep the bare id (with version) and build
        // the canonical links from it.
        let bare = self
            .id
            .rsplit("/abs/")
            .next()
            .unwrap_or(&self.id)
            .to_string();
        let pdf = if self.pdf.is_empty() {
            format!("https://arxiv.org/pdf/{bare}")
        } else {
            self.pdf
        };
        let mut obj = json!({
            "id": bare,
            "title": collapse(&self.title),
            "authors": self.authors,
            "published": self.published,
            "updated": self.updated,
            "primary_category": self.primary,
            "categories": self.categories,
            "abs_url": format!("https://arxiv.org/abs/{bare}"),
            "pdf_url": pdf,
            "summary": collapse(&self.summary),
        });
        let map = obj.as_object_mut().expect("object");
        // Empty optionals are omitted: a compact result costs fewer tokens and is
        // what "blitzschnell" means for the model too.
        for (key, value) in [
            ("comment", self.comment),
            ("journal_ref", self.journal_ref),
            ("doi", self.doi),
        ] {
            if !value.trim().is_empty() {
                map.insert(key.to_string(), json!(collapse(&value)));
            }
        }
        // A withdrawn paper keeps its metadata but must not be read as a result.
        let lower = self.summary.to_lowercase();
        if lower.contains("withdrawn") || lower.contains("retracted") {
            map.insert("withdrawn".to_string(), json!(true));
        }
        obj
    }
}

/// Collapse the pretty-printed XML whitespace of a text node into one line.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn attr(e: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// Parse an arXiv Atom feed into JSON entries. Namespaces are ignored (the
/// `arxiv:` prefix would otherwise hide `primary_category`, `comment`, …).
fn parse_feed(xml: &str) -> Result<Vec<Value>, String> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    let mut cur: Option<Entry> = None;
    let mut text = String::new();
    let mut in_author = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_string();
                match name.as_str() {
                    "entry" => cur = Some(Entry::default()),
                    "author" => in_author = true,
                    _ => {}
                }
                text.clear();
            }
            Ok(Event::Empty(e)) => {
                let name = e.local_name().as_ref().to_string();
                if let Some(c) = cur.as_mut() {
                    match name.as_str() {
                        "category" => {
                            if let Some(term) = attr(&e, "term") {
                                if !c.categories.contains(&term) {
                                    c.categories.push(term);
                                }
                            }
                        }
                        "primary_category" => {
                            if let Some(term) = attr(&e, "term") {
                                c.primary = term;
                            }
                        }
                        "link" => {
                            let is_pdf = matches!(attr(&e, "title").as_deref(), Some("pdf"))
                                || matches!(attr(&e, "type").as_deref(), Some("application/pdf"));
                            if is_pdf {
                                if let Some(href) = attr(&e, "href") {
                                    c.pdf = href;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Text(t)) => {
                // 0.42 stores the text as `&str`; `xml10_content` resolves the XML
                // entities (`&amp;` and friends) for us.
                text.push_str(&t.xml10_content());
            }
            Ok(Event::End(e)) => {
                let name = e.local_name().as_ref().to_string();
                if let Some(c) = cur.as_mut() {
                    let v = text.trim().to_string();
                    match name.as_str() {
                        "id" => c.id = v,
                        "title" => c.title = v,
                        "summary" => c.summary = v,
                        "published" => c.published = v,
                        "updated" => c.updated = v,
                        "name" if in_author => c.authors.push(collapse(&v)),
                        "comment" => c.comment = v,
                        "journal_ref" => c.journal_ref = v,
                        "doi" => c.doi = v,
                        _ => {}
                    }
                }
                if name == "author" {
                    in_author = false;
                }
                if name == "entry" {
                    if let Some(c) = cur.take() {
                        out.push(c.into_json());
                    }
                }
                text.clear();
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("arXiv XML: {e}")),
        }
        buf.clear();
    }
    Ok(out)
}

/// arXiv answers a malformed query with HTTP 200 and a single entry whose id is
/// an errors URL — surface it instead of returning it as a paper.
fn arxiv_error_entry(entries: &[Value]) -> Option<String> {
    if entries.len() != 1 {
        return None;
    }
    let id = entries[0]["id"].as_str().unwrap_or("");
    id.contains("api/errors").then(|| {
        entries[0]["summary"]
            .as_str()
            .unwrap_or("malformed arXiv query")
            .to_string()
    })
}

// ─── Actions ─────────────────────────────────────────────────────────────

async fn do_search(args: &Value) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if query.is_empty() {
        return Err("query is required for action=search".into());
    }
    let max = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(8)
        .clamp(1, 50);
    let start = args.get("start").and_then(|v| v.as_u64()).unwrap_or(0);
    let sort = args
        .get("sort")
        .and_then(|v| v.as_str())
        .unwrap_or("relevance");
    let category = args
        .get("category")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut q = to_search_query(query);
    if let Some(cat) = category {
        q = format!("cat:{}+AND+{}", encode_query(cat), q);
    }
    // `relevance` is arXiv's default but passing it keeps the URL self-describing.
    let (by, order) = match sort {
        "date" | "submitted" => ("submittedDate", "descending"),
        "updated" => ("lastUpdatedDate", "descending"),
        _ => ("relevance", "descending"),
    };
    let url = format!(
        "{ARXIV_API}?search_query={q}&start={start}&max_results={max}&sortBy={by}&sortOrder={order}"
    );

    let xml = arxiv_get(&url).await?;
    let entries = parse_feed(&xml)?;
    if let Some(err) = arxiv_error_entry(&entries) {
        return Err(format!("arXiv rejected the query: {err}"));
    }
    Ok(json!({ "action": "search", "count": entries.len(), "results": entries }).to_string())
}

async fn do_get(args: &Value) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("id is required (arXiv id or comma-separated list)")?;
    let url = format!("{ARXIV_API}?id_list={}", encode_query(id));
    let xml = arxiv_get(&url).await?;
    let entries = parse_feed(&xml)?;
    if let Some(err) = arxiv_error_entry(&entries) {
        return Err(format!("arXiv rejected the id list: {err}"));
    }
    if entries.is_empty() {
        return Err(format!("no arXiv paper found for id '{id}'"));
    }
    Ok(json!({ "action": "get", "count": entries.len(), "results": entries }).to_string())
}

#[derive(Clone, Copy)]
enum S2Kind {
    Stats,
    References,
    CitedBy,
}

async fn do_s2(args: &Value, kind: S2Kind) -> Result<String, String> {
    let raw = args
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("id is required")?;
    // The first id is enough: these actions are about ONE paper.
    let id = strip_version(raw.split(',').next().unwrap_or(raw));
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .clamp(1, 50);

    let (action, path) = match kind {
        S2Kind::Stats => (
            "citations",
            format!(
                "/arXiv:{id}?fields=title,year,citationCount,influentialCitationCount,referenceCount,isOpenAccess,openAccessPdf,externalIds"
            ),
        ),
        S2Kind::References => (
            "references",
            format!(
                "/arXiv:{id}/references?fields=title,authors,year,citationCount&limit={limit}"
            ),
        ),
        S2Kind::CitedBy => (
            "cited_by",
            format!("/arXiv:{id}/citations?fields=title,authors,year,citationCount&limit={limit}"),
        ),
    };
    let body = s2_get(&path).await?;
    Ok(json!({ "action": action, "paper": id, "data": body }).to_string())
}

// ─── Tool ────────────────────────────────────────────────────────────────

lazy_static::lazy_static! {
    static ref ARXIV_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["search", "get", "citations", "references", "cited_by"],
                "description": "search: find papers; get: metadata by id; citations: citation/impact stats; references: what a paper cites; cited_by: who cites it"
            },
            "query": {
                "type": "string",
                "description": "search: text, or arXiv syntax (all:, ti:, au:, abs:, cat:, co:) with AND/OR/ANDNOT and \"exact phrases\""
            },
            "id": {
                "type": "string",
                "description": "get/citations/references/cited_by: arXiv id(s), comma-separated (2402.03300 or hep-th/0601001)"
            },
            "max_results": {
                "type": "integer", "minimum": 1, "maximum": 50,
                "description": "search: number of results (default 8)"
            },
            "start": {
                "type": "integer", "minimum": 0,
                "description": "search: result offset, for paging"
            },
            "sort": {
                "type": "string", "enum": ["relevance", "date", "updated"],
                "description": "search: order (default relevance)"
            },
            "category": {
                "type": "string",
                "description": "search: restrict to an arXiv category, e.g. cs.LG, cs.CL, stat.ML"
            },
            "limit": {
                "type": "integer", "minimum": 1, "maximum": 50,
                "description": "references/cited_by: rows (default 10)"
            }
        },
        "required": ["action"]
    }));
}

pub struct ArxivTool;

#[async_trait]
impl AsyncTool for ArxivTool {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "arxiv",
            description: "Search and analyse arXiv papers: metadata, abstracts, citations. No key.",
            emoji: "📚",
            toolset: "web",
            is_async: true,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &ARXIV_SCHEMA
    }

    async fn call_async(&self, args: Value, state: std::sync::Arc<AppState>) -> ToolResult {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("search")
            .to_string();

        // The cache is keyed on the exact arguments, so a repeated question costs
        // one hash lookup instead of a round trip (and never trips the rate limit).
        let key = cache_key(&action, &args.to_string());
        if let Some(hit) = cache_get(&state, &key) {
            return hit;
        }

        let result = match action.as_str() {
            "search" => do_search(&args).await,
            "get" => do_get(&args).await,
            "citations" => do_s2(&args, S2Kind::Stats).await,
            "references" => do_s2(&args, S2Kind::References).await,
            "cited_by" => do_s2(&args, S2Kind::CitedBy).await,
            other => Err(format!(
                "unknown action '{other}' — use search, get, citations, references or cited_by"
            )),
        };

        match result {
            Ok(out) => {
                cache_put(&state, &key, &out);
                out
            }
            Err(e) => tool_error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_query_searches_all_fields() {
        assert_eq!(to_search_query("chain of thought"), "all:chain+of+thought");
        // Already-qualified queries are passed through, operators intact.
        assert_eq!(to_search_query("au:hinton"), "au:hinton");
        assert_eq!(
            to_search_query("ti:\"chain of thought\""),
            "ti:\"chain+of+thought\""
        );
        assert_eq!(
            to_search_query("all:GPT OR all:BERT"),
            "all:GPT+OR+all:BERT"
        );
    }

    #[test]
    fn a_query_cannot_inject_a_url_parameter() {
        let q = to_search_query("cats & dogs #1");
        assert!(!q.contains('&'), "{q}");
        assert!(!q.contains('#'), "{q}");
        assert!(q.starts_with("all:cats+"), "{q}");
    }

    #[test]
    fn version_suffix_is_stripped_for_semantic_scholar() {
        assert_eq!(strip_version("2402.03300v3"), "2402.03300");
        assert_eq!(strip_version("2402.03300"), "2402.03300");
        assert_eq!(
            strip_version("http://arxiv.org/abs/2402.03300v1"),
            "2402.03300"
        );
        // An old-style id with a slash keeps its shape (no trailing vN).
        assert_eq!(strip_version("hep-th/0601001"), "hep-th/0601001");
    }

    /// A realistic slice of an arXiv Atom feed: namespaces on the tag names, the
    /// abstract spread over lines, several authors, several categories, a pdf
    /// link and the optional journal/doi fields.
    const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:arxiv="http://arxiv.org/schemas/atom">
  <title type="html">ArXiv Query: search_query=all:attention</title>
  <entry>
    <id>http://arxiv.org/abs/1706.03762v7</id>
    <updated>2023-08-02T00:00:00Z</updated>
    <published>2017-06-12T17:57:34Z</published>
    <title>Attention Is All You Need</title>
    <summary>  The dominant sequence transduction models are based on complex
recurrent or convolutional neural networks. We propose a new
simple network architecture, the Transformer.
    </summary>
    <author><name>Ashish Vaswani</name></author>
    <author><name>Noam Shazeer</name></author>
    <arxiv:comment>NeurIPS 2017</arxiv:comment>
    <arxiv:journal_ref>Advances in Neural Information Processing Systems 30</arxiv:journal_ref>
    <arxiv:doi>10.5555/3295222.3295349</arxiv:doi>
    <link href="http://arxiv.org/abs/1706.03762v7" rel="alternate" type="text/html"/>
    <link href="http://arxiv.org/pdf/1706.03762v7" rel="related" title="pdf" type="application/pdf"/>
    <arxiv:primary_category term="cs.CL" scheme="http://arxiv.org/schemas/atom"/>
    <category term="cs.CL" scheme="http://arxiv.org/schemas/atom"/>
    <category term="cs.LG" scheme="http://arxiv.org/schemas/atom"/>
  </entry>
</feed>"#;

    #[test]
    fn a_feed_entry_becomes_a_compact_json_paper() {
        let entries = parse_feed(FEED).expect("parse");
        assert_eq!(entries.len(), 1);
        let p = &entries[0];
        assert_eq!(p["id"], "1706.03762v7");
        assert_eq!(p["title"], "Attention Is All You Need");
        assert_eq!(p["authors"], json!(["Ashish Vaswani", "Noam Shazeer"]));
        assert_eq!(p["primary_category"], "cs.CL");
        assert_eq!(p["categories"], json!(["cs.CL", "cs.LG"]));
        assert_eq!(p["published"], "2017-06-12T17:57:34Z");
        assert_eq!(p["comment"], "NeurIPS 2017");
        assert_eq!(p["doi"], "10.5555/3295222.3295349");
        assert_eq!(p["pdf_url"], "http://arxiv.org/pdf/1706.03762v7");
        assert_eq!(p["abs_url"], "https://arxiv.org/abs/1706.03762v7");
        // The abstract was pretty-printed over lines: it must arrive as one line.
        let summary = p["summary"].as_str().unwrap();
        assert!(summary.starts_with("The dominant sequence"), "{summary}");
        assert!(!summary.contains('\n'), "{summary}");
        assert!(p.get("withdrawn").is_none(), "not withdrawn");
    }

    #[test]
    fn a_withdrawn_paper_is_flagged() {
        let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry>
            <id>http://arxiv.org/abs/2402.00001v2</id>
            <title>Some Paper</title>
            <summary>This paper has been withdrawn by the authors.</summary>
        </entry></feed>"#;
        let entries = parse_feed(xml).expect("parse");
        assert_eq!(entries[0]["withdrawn"], true);
    }

    #[test]
    fn an_arxiv_error_feed_is_recognised() {
        let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry>
            <id>http://arxiv.org/api/errors#incorrect_id_format_for_foo</id>
            <title>Error</title>
            <summary>incorrect id format for foo</summary>
        </entry></feed>"#;
        let entries = parse_feed(xml).expect("parse");
        assert_eq!(
            arxiv_error_entry(&entries).as_deref(),
            Some("incorrect id format for foo")
        );
    }
}
