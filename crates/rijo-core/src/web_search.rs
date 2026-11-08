//! Keyless web-search providers for `web_search`.
//!
//! A **flat list** of small functions over free, key-less, JSON endpoints. Every
//! endpoint below answered `200` with real data on 2026-10-11 (recorded in
//! `docs/60-research/quellen/rijo-web/measurements-2026-10-11.md` §6):
//! Wikipedia, crates.io, MDN, Stack Exchange, npm, grep.app (code across public
//! GitHub), OpenAlex, Hacker News (Algolia) and Wiby — plus the built-in
//! DuckDuckGo Lite scrape, kept but treated as **best-effort**: it answered
//! HTTP 202 / anti-bot on 6 of 6 attempts from this network, and for a real
//! headless browser too.
//!
//! Design rules (ADR 0022):
//! - **One shape**: every provider returns `{title, url, description}`.
//! - **Fail-open**: a broken provider yields `Err` and is *named* in the caller's
//!   note; it can never fail the whole search.
//! - **No framework**: one `const` table, no traits, no query router, no weighted
//!   fusion. All providers are asked in parallel (they are fast and free) and
//!   merged by canonical URL, ranked by how many providers agreed.
//! - **Parsers are separate functions** (`parse_*`), so each is unit-testable
//!   from a saved response without touching the network.

use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

/// A provider call: `(client, base, query, limit, timeout) -> hits`.
pub type ProviderFn = fn(
    &reqwest::Client,
    &str,
    &str,
    usize,
    Duration,
) -> BoxFuture<'static, Result<Vec<Value>, String>>;

/// The provider table: name, default base URL, call.
///
/// Order is deliberate — it is the **source priority** used to rank hits that no
/// other provider agreed on (code and documentation first, then encyclopedic and
/// scholarly, then community, then the scraped engine last):
/// crates-io → mdn → stackexchange → npm → grep-app → wikipedia → openalex →
/// hn → wiby → duckduckgo.
pub const PROVIDERS: &[(&str, &str, ProviderFn)] = &[
    ("crates-io", "https://crates.io", p_crates_io),
    ("mdn", "https://developer.mozilla.org", p_mdn),
    (
        "stackexchange",
        "https://api.stackexchange.com",
        p_stackexchange,
    ),
    ("npm", "https://registry.npmjs.org", p_npm),
    ("grep-app", "https://grep.app", p_grep_app),
    ("wikipedia", WP_API, p_wikipedia),
    ("openalex", "https://api.openalex.org", p_openalex),
    ("hn", "https://hn.algolia.com", p_hn),
    ("wiby", "https://wiby.me", p_wiby),
    ("duckduckgo", crate::meta_search::DDG_ENDPOINT, p_duckduckgo),
];

/// A provider's position in [`PROVIDERS`] — lower is a higher source priority.
fn provider_priority(name: &str) -> usize {
    PROVIDERS
        .iter()
        .position(|(n, _, _)| *n == name)
        .unwrap_or(usize::MAX)
}

/// The result of one whole search: the merged hits plus everything the caller
/// needs to keep its cooldown bookkeeping honest.
pub struct SearchRun {
    /// Merged, ranked hits (already capped to `limit`).
    pub hits: Vec<Value>,
    /// Providers that returned at least one hit, sorted.
    pub contributors: Vec<&'static str>,
    /// Provider name + reason, for every provider that failed.
    pub failures: Vec<(&'static str, String)>,
    /// Providers not asked because the caller put them in cooldown.
    pub skipped: Vec<&'static str>,
}

/// Ask every provider (except `skip`) in parallel and merge the results.
pub async fn search_all(
    client: &reqwest::Client,
    endpoints: &BTreeMap<String, String>,
    query: &str,
    limit: usize,
    timeout: Duration,
    skip: &[&str],
) -> SearchRun {
    let mut skipped: Vec<&'static str> = Vec::new();
    let mut futs = Vec::new();
    for (name, default_base, call) in PROVIDERS {
        if skip.contains(name) {
            skipped.push(name);
            continue;
        }
        let base = endpoints
            .get(*name)
            .map(String::as_str)
            .unwrap_or(default_base)
            .to_string();
        let fut = call(client, &base, query, limit, timeout);
        futs.push(async move { (*name, fut.await) });
    }

    let results = futures_util::future::join_all(futs).await;

    let mut lists: Vec<(&'static str, Vec<Value>)> = Vec::new();
    let mut failures: Vec<(&'static str, String)> = Vec::new();
    for (name, res) in results {
        match res {
            Ok(hits) if !hits.is_empty() => lists.push((name, hits)),
            Ok(_) => {} // answered, simply nothing to offer
            Err(e) => failures.push((name, e)),
        }
    }

    let mut contributors: Vec<&'static str> = lists.iter().map(|(n, _)| *n).collect();
    contributors.sort_unstable();

    SearchRun {
        hits: merge(lists, limit),
        contributors,
        failures,
        skipped,
    }
}

// ─── Merge / dedup ────────────────────────────────────────────────────────

/// A dedup key for a result URL: lowercased host without `www.`, the path
/// without a trailing slash, and non-tracking query params (sorted) — plus no
/// fragment. Two providers returning the same article collapse to one hit.
pub fn canonical_url(raw: &str) -> String {
    let Ok(u) = url::Url::parse(raw.trim()) else {
        return raw.trim().to_lowercase();
    };
    let host = u.host_str().unwrap_or("").to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    let path = u.path().trim_end_matches('/');
    let mut params: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| {
            let k = k.to_lowercase();
            !k.starts_with("utm_") && k != "ref" && k != "fbclid"
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    params.sort();
    let q: String = params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    if q.is_empty() {
        format!("{host}{path}")
    } else {
        format!("{host}{path}?{q}")
    }
}

/// Merge provider hit lists: one hit per canonical URL. `provider` is the first
/// contributor in [`PROVIDERS`] order (the highest-priority provider that found
/// it); `sources` lists every contributor, sorted.
///
/// Ranked by: **how many providers agreed**, then the **tier** (a provider's #1
/// always beats anyone's #3), then the static **source priority**, then the
/// canonical URL — the last only so the output is deterministic.
pub fn merge(lists: Vec<(&'static str, Vec<Value>)>, limit: usize) -> Vec<Value> {
    struct Acc {
        count: usize,
        best_rank: usize,
        /// First contributor seen — the citation `provider`.
        primary: &'static str,
        priority: usize,
        sources: Vec<&'static str>,
        hit: Value,
    }
    fn desc_len(hit: &Value) -> usize {
        hit.get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.chars().count())
            .unwrap_or(0)
    }

    let mut acc: HashMap<String, Acc> = HashMap::new();
    for (provider, hits) in &lists {
        for (rank, h) in hits.iter().enumerate() {
            let url = h.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let key = canonical_url(url);
            if key.is_empty() {
                continue;
            }
            let entry = acc.entry(key).or_insert_with(|| Acc {
                count: 0,
                best_rank: usize::MAX,
                primary: provider,
                priority: provider_priority(provider),
                sources: Vec::new(),
                hit: h.clone(),
            });
            entry.count += 1;
            entry.best_rank = entry.best_rank.min(rank);
            if !entry.sources.contains(provider) {
                entry.sources.push(provider);
            }
            // Keep the most informative copy of a shared URL.
            if desc_len(h) > desc_len(&entry.hit) {
                entry.hit = h.clone();
            }
        }
    }

    let mut rows: Vec<(String, Acc)> = acc.into_iter().collect();
    rows.sort_by(|(ka, a), (kb, b)| {
        b.count
            .cmp(&a.count)
            .then(a.best_rank.cmp(&b.best_rank))
            .then(a.priority.cmp(&b.priority))
            .then(ka.cmp(kb))
    });

    rows.into_iter()
        .take(limit)
        .map(|(_, mut a)| {
            a.sources.sort_unstable();
            a.hit["provider"] = json!(a.primary);
            a.hit["sources"] = json!(a.sources);
            a.hit
        })
        .collect()
}

// ─── Shared helpers ───────────────────────────────────────────────────────

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn get_json(client: &reqwest::Client, url: &str, timeout: Duration) -> Result<Value, String> {
    let resp = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

fn hit(title: &str, url: &str, description: &str) -> Value {
    json!({
        "title": title.trim(),
        "url": url.trim(),
        "description": description.trim(),
    })
}

/// Strip HTML tags and collapse whitespace. Wikipedia snippets wrap the match in
/// `<span class="searchmatch">`; grep.app returns whole highlighted HTML tables.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ─── Wikipedia ────────────────────────────────────────────────────────────

const WP_API: &str = "https://en.wikipedia.org/w/api.php";

fn p_wikipedia(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
    timeout: Duration,
) -> BoxFuture<'static, Result<Vec<Value>, String>> {
    let client = client.clone();
    let url = format!(
        "{base}?action=query&list=search&srsearch={}&srlimit={limit}&format=json",
        enc(query)
    );
    Box::pin(async move {
        let v = get_json(&client, &url, timeout).await?;
        Ok(parse_wikipedia(&v, limit))
    })
}

/// `{"query":{"search":[{"title","snippet"}]}}`
pub fn parse_wikipedia(v: &Value, limit: usize) -> Vec<Value> {
    array(v, &["query", "search"])
        .iter()
        .take(limit)
        .filter_map(|r| {
            let title = r["title"].as_str()?;
            Some(hit(
                title,
                &format!("https://en.wikipedia.org/wiki/{}", title.replace(' ', "_")),
                &strip_tags(r["snippet"].as_str().unwrap_or("")),
            ))
        })
        .collect()
}

// ─── crates.io ────────────────────────────────────────────────────────────

fn p_crates_io(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
    timeout: Duration,
) -> BoxFuture<'static, Result<Vec<Value>, String>> {
    let client = client.clone();
    let url = format!("{base}/api/v1/crates?q={}&per_page={limit}", enc(query));
    Box::pin(async move {
        let v = get_json(&client, &url, timeout).await?;
        Ok(parse_crates_io(&v, limit))
    })
}

/// `{"crates":[{"name","max_version","description"}]}`
pub fn parse_crates_io(v: &Value, limit: usize) -> Vec<Value> {
    array(v, &["crates"])
        .iter()
        .take(limit)
        .filter_map(|c| {
            let name = c["name"].as_str()?;
            let version = c["max_version"].as_str().unwrap_or("");
            Some(hit(
                &format!("{name} {version}"),
                &format!("https://crates.io/crates/{name}"),
                c["description"].as_str().unwrap_or(""),
            ))
        })
        .collect()
}

// ─── MDN ──────────────────────────────────────────────────────────────────

fn p_mdn(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
    timeout: Duration,
) -> BoxFuture<'static, Result<Vec<Value>, String>> {
    let client = client.clone();
    let url = format!("{base}/api/v1/search?q={}&locale=en-US", enc(query));
    Box::pin(async move {
        let v = get_json(&client, &url, timeout).await?;
        Ok(parse_mdn(&v, limit))
    })
}

/// `{"documents":[{"mdn_url","title","summary"}]}`
pub fn parse_mdn(v: &Value, limit: usize) -> Vec<Value> {
    array(v, &["documents"])
        .iter()
        .take(limit)
        .filter_map(|d| {
            let path = d["mdn_url"].as_str()?;
            let url = if path.starts_with('/') {
                format!("https://developer.mozilla.org{path}")
            } else {
                path.to_string()
            };
            Some(hit(
                d["title"].as_str().unwrap_or(""),
                &url,
                d["summary"].as_str().unwrap_or(""),
            ))
        })
        .collect()
}

// ─── Stack Exchange ───────────────────────────────────────────────────────

fn p_stackexchange(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
    timeout: Duration,
) -> BoxFuture<'static, Result<Vec<Value>, String>> {
    let client = client.clone();
    let url = format!(
        "{base}/2.3/search/advanced?order=desc&sort=relevance&q={}&site=stackoverflow&pagesize={limit}",
        enc(query)
    );
    Box::pin(async move {
        let v = get_json(&client, &url, timeout).await?;
        Ok(parse_stackexchange(&v, limit))
    })
}

/// `{"items":[{"title","link","score","answer_count","is_answered"}]}`
pub fn parse_stackexchange(v: &Value, limit: usize) -> Vec<Value> {
    array(v, &["items"])
        .iter()
        .take(limit)
        .filter_map(|i| {
            let link = i["link"].as_str()?;
            let score = i["score"].as_i64().unwrap_or(0);
            let answers = i["answer_count"].as_i64().unwrap_or(0);
            let answered = i["is_answered"].as_bool().unwrap_or(false);
            let state = if answered { "answered" } else { "unanswered" };
            Some(hit(
                i["title"].as_str().unwrap_or(""),
                link,
                &format!("score {score}, {answers} answers, {state}"),
            ))
        })
        .collect()
}

// ─── npm ──────────────────────────────────────────────────────────────────

fn p_npm(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
    timeout: Duration,
) -> BoxFuture<'static, Result<Vec<Value>, String>> {
    let client = client.clone();
    let url = format!("{base}/-/v1/search?text={}&size={limit}", enc(query));
    Box::pin(async move {
        let v = get_json(&client, &url, timeout).await?;
        Ok(parse_npm(&v, limit))
    })
}

/// `{"objects":[{"package":{"name","description","links":{"npm"}}}]}`
pub fn parse_npm(v: &Value, limit: usize) -> Vec<Value> {
    array(v, &["objects"])
        .iter()
        .take(limit)
        .filter_map(|o| {
            let p = &o["package"];
            let name = p["name"].as_str()?;
            let url = p["links"]["npm"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| format!("https://www.npmjs.com/package/{name}"));
            Some(hit(name, &url, p["description"].as_str().unwrap_or("")))
        })
        .collect()
}

// ─── grep.app (code across public GitHub) ─────────────────────────────────

fn p_grep_app(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
    timeout: Duration,
) -> BoxFuture<'static, Result<Vec<Value>, String>> {
    let client = client.clone();
    let url = format!("{base}/api/search?q={}", enc(query));
    Box::pin(async move {
        let v = get_json(&client, &url, timeout).await?;
        Ok(parse_grep_app(&v, limit))
    })
}

/// `{"hits":{"hits":[{"repo","branch","path","content":{"snippet"}}]}}`
pub fn parse_grep_app(v: &Value, limit: usize) -> Vec<Value> {
    array(v, &["hits", "hits"])
        .iter()
        .take(limit)
        .filter_map(|h| {
            let repo = h["repo"].as_str()?;
            let path = h["path"].as_str().unwrap_or("");
            let branch = h["branch"].as_str().unwrap_or("HEAD");
            Some(hit(
                &format!("{repo}: {path}"),
                &format!("https://github.com/{repo}/blob/{branch}/{path}"),
                &strip_tags(h["content"]["snippet"].as_str().unwrap_or("")),
            ))
        })
        .collect()
}

// ─── OpenAlex ─────────────────────────────────────────────────────────────

fn p_openalex(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
    timeout: Duration,
) -> BoxFuture<'static, Result<Vec<Value>, String>> {
    let client = client.clone();
    let url = format!("{base}/works?search={}&per-page={limit}", enc(query));
    Box::pin(async move {
        let v = get_json(&client, &url, timeout).await?;
        Ok(parse_openalex(&v, limit))
    })
}

/// `{"results":[{"title","doi","publication_year","primary_location":{...}}]}`
pub fn parse_openalex(v: &Value, limit: usize) -> Vec<Value> {
    array(v, &["results"])
        .iter()
        .take(limit)
        .filter_map(|w| {
            let title = w["title"]
                .as_str()
                .or_else(|| w["display_name"].as_str())
                .unwrap_or("");
            let url = w["doi"]
                .as_str()
                .or_else(|| w["primary_location"]["landing_page_url"].as_str())
                .or_else(|| w["id"].as_str())?;
            let year = w["publication_year"].as_i64().unwrap_or(0);
            let oa = if w["primary_location"]["is_oa"].as_bool().unwrap_or(false) {
                "open access"
            } else {
                "closed access"
            };
            Some(hit(title, url, &format!("{year}, {oa}")))
        })
        .collect()
}

// ─── Hacker News (Algolia) ────────────────────────────────────────────────

fn p_hn(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
    timeout: Duration,
) -> BoxFuture<'static, Result<Vec<Value>, String>> {
    let client = client.clone();
    let url = format!(
        "{base}/api/v1/search?query={}&hitsPerPage={limit}",
        enc(query)
    );
    Box::pin(async move {
        let v = get_json(&client, &url, timeout).await?;
        Ok(parse_hn(&v, limit))
    })
}

/// `{"hits":[{"title","url","objectID","points","num_comments"}]}`
pub fn parse_hn(v: &Value, limit: usize) -> Vec<Value> {
    array(v, &["hits"])
        .iter()
        .take(limit)
        .filter_map(|h| {
            let object_id = h["objectID"].as_str()?;
            let url = h["url"]
                .as_str()
                .filter(|u| !u.is_empty())
                .map(String::from)
                .unwrap_or_else(|| format!("https://news.ycombinator.com/item?id={object_id}"));
            let title = h["title"]
                .as_str()
                .filter(|t| !t.is_empty())
                .unwrap_or(url.as_str());
            let points = h["points"].as_i64().unwrap_or(0);
            let comments = h["num_comments"].as_i64().unwrap_or(0);
            Some(hit(
                title,
                &url,
                &format!("{points} points, {comments} comments"),
            ))
        })
        .collect()
}

// ─── Wiby (small "old web" index, JSON, no key) ───────────────────────────

fn p_wiby(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
    timeout: Duration,
) -> BoxFuture<'static, Result<Vec<Value>, String>> {
    let client = client.clone();
    let url = format!("{base}/json/?q={}", enc(query));
    Box::pin(async move {
        let v = get_json(&client, &url, timeout).await?;
        Ok(parse_wiby(&v, limit))
    })
}

/// A top-level array: `[{"URL","Title","Snippet"}]`. Wiby ignores a limit
/// parameter, so the cap happens here.
pub fn parse_wiby(v: &Value, limit: usize) -> Vec<Value> {
    v.as_array()
        .map(|a| {
            a.iter()
                .take(limit)
                .filter_map(|r| {
                    let url = r["URL"].as_str()?;
                    Some(hit(
                        r["Title"].as_str().unwrap_or(""),
                        url,
                        r["Snippet"].as_str().unwrap_or(""),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ─── DuckDuckGo Lite (built-in scrape, best-effort) ───────────────────────

fn p_duckduckgo(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
    timeout: Duration,
) -> BoxFuture<'static, Result<Vec<Value>, String>> {
    let client = client.clone();
    let (base, query) = (base.to_string(), query.to_string());
    Box::pin(
        async move { crate::meta_search::duckduckgo(&client, &base, &query, limit, timeout).await },
    )
}

/// Borrow a nested array, or an empty slice when the path is missing/not an
/// array. Providers must never panic on a changed response shape.
fn array<'a>(v: &'a Value, path: &[&str]) -> &'a [Value] {
    let mut cur = v;
    for key in path {
        cur = match cur.get(key) {
            Some(next) => next,
            None => return &[],
        };
    }
    cur.as_array().map(|a| a.as_slice()).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures are trimmed copies of responses actually fetched on 2026-10-11
    // (the raw bodies live in docs/60-research/quellen/rijo-web/probe3-4/).

    #[test]
    fn wikipedia_parses_title_and_unwraps_the_searchmatch_span() {
        let v = json!({"query": {"search": [
            {"title": "Rust (programming language)",
             "snippet": "Rust is a <span class=\"searchmatch\">general-purpose</span> language."}
        ]}});
        let hits = parse_wikipedia(&v, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0]["url"],
            json!("https://en.wikipedia.org/wiki/Rust_(programming_language)")
        );
        assert_eq!(
            hits[0]["description"],
            json!("Rust is a general-purpose language.")
        );
    }

    #[test]
    fn crates_io_parses_name_and_version() {
        let v = json!({"crates": [
            {"name": "pyo3-async-runtimes", "max_version": "0.29.0",
             "description": "PyO3 bridges from Rust runtimes to Python's Asyncio library"}
        ]});
        let hits = parse_crates_io(&v, 5);
        assert_eq!(hits[0]["title"], json!("pyo3-async-runtimes 0.29.0"));
        assert_eq!(
            hits[0]["url"],
            json!("https://crates.io/crates/pyo3-async-runtimes")
        );
    }

    #[test]
    fn mdn_absolutises_the_documentation_url() {
        let v = json!({"documents": [
            {"mdn_url": "/en-US/docs/Web/JavaScript/Reference/Global_Objects/Array",
             "title": "Array", "summary": "The Array object ..."}
        ]});
        let hits = parse_mdn(&v, 5);
        assert_eq!(
            hits[0]["url"],
            json!("https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Array")
        );
    }

    #[test]
    fn stackexchange_reports_score_and_answer_state() {
        let v = json!({"items": [
            {"title": "Fighting with the Rust borrow checker",
             "link": "https://stackoverflow.com/questions/32443697/fighting-with-the-rust-borrow-checker",
             "score": 2, "answer_count": 1, "is_answered": true}
        ]});
        let hits = parse_stackexchange(&v, 5);
        assert_eq!(
            hits[0]["description"],
            json!("score 2, 1 answers, answered")
        );
        assert!(hits[0]["url"]
            .as_str()
            .unwrap()
            .contains("stackoverflow.com/questions/"));
    }

    #[test]
    fn npm_uses_the_package_link() {
        let v = json!({"objects": [{"package": {
            "name": "react", "description": "React is a JavaScript library ...",
            "links": {"npm": "https://www.npmjs.com/package/react"}
        }}]});
        let hits = parse_npm(&v, 5);
        assert_eq!(hits[0]["title"], json!("react"));
        assert_eq!(hits[0]["url"], json!("https://www.npmjs.com/package/react"));
    }

    #[test]
    fn grep_app_builds_a_github_url_and_strips_the_html_snippet() {
        let v = json!({"hits": {"hits": [{
            "repo": "rustdesk/rustdesk", "branch": "master", "path": "src/flutter_ffi.rs",
            "content": {"snippet": "<table><tr><td><div>934</div></td><td><span class=\"k\">pub</span> fn x()</td></tr></table>"}
        }]}});
        let hits = parse_grep_app(&v, 5);
        assert_eq!(
            hits[0]["url"],
            json!("https://github.com/rustdesk/rustdesk/blob/master/src/flutter_ffi.rs")
        );
        assert!(!hits[0]["description"].as_str().unwrap().contains('<'));
        assert!(hits[0]["description"].as_str().unwrap().contains("fn x()"));
    }

    #[test]
    fn openalex_prefers_the_doi_and_falls_back_to_the_landing_page() {
        let v = json!({"results": [
            {"title": "Active Retrieval Augmented Generation",
             "doi": "https://doi.org/10.18653/v1/2023.emnlp-main.495",
             "publication_year": 2023,
             "primary_location": {"landing_page_url": "https://doi.org/10.18653/v1/2023.emnlp-main.495", "is_oa": true}},
            {"display_name": "No DOI here",
             "primary_location": {"landing_page_url": "https://example.org/paper", "is_oa": false}}
        ]});
        let hits = parse_openalex(&v, 5);
        assert_eq!(
            hits[0]["url"],
            json!("https://doi.org/10.18653/v1/2023.emnlp-main.495")
        );
        assert_eq!(hits[0]["description"], json!("2023, open access"));
        assert_eq!(hits[1]["url"], json!("https://example.org/paper"));
        assert_eq!(hits[1]["description"], json!("0, closed access"));
    }

    #[test]
    fn hn_falls_back_to_the_item_page_for_ask_hn() {
        let v = json!({"hits": [
            {"objectID": "22238335", "title": "Why Discord is switching from Go to Rust",
             "url": "https://blog.discordapp.com/x", "points": 1582, "num_comments": 300},
            {"objectID": "1", "title": "Ask HN: something", "url": null, "points": 5, "num_comments": 2}
        ]});
        let hits = parse_hn(&v, 5);
        assert_eq!(hits[0]["url"], json!("https://blog.discordapp.com/x"));
        assert_eq!(
            hits[1]["url"],
            json!("https://news.ycombinator.com/item?id=1")
        );
        assert_eq!(hits[0]["description"], json!("1582 points, 300 comments"));
    }

    #[test]
    fn wiby_parses_the_top_level_array_and_respects_the_limit() {
        let v = json!([
            {"URL": "http://lambdalemon.gay/posts/grist-lens", "Title": "Lenses in Rust",
             "Snippet": " -- Home Blog Posts Lenses in Rust", "Description": "In"}
        ]);
        let hits = parse_wiby(&v, 5);
        assert_eq!(hits[0]["title"], json!("Lenses in Rust"));
        assert_eq!(
            hits[0]["url"],
            json!("http://lambdalemon.gay/posts/grist-lens")
        );
        assert_eq!(parse_wiby(&v, 0).len(), 0);
    }

    #[test]
    fn a_changed_response_shape_yields_no_hits_instead_of_a_panic() {
        assert!(parse_wikipedia(&json!({"error": "nope"}), 5).is_empty());
        assert!(parse_crates_io(&json!([]), 5).is_empty());
        assert!(parse_grep_app(&json!({"hits": "not an object"}), 5).is_empty());
        assert!(parse_openalex(&json!(null), 5).is_empty());
        assert!(parse_wiby(&json!({}), 5).is_empty());
    }

    #[test]
    fn canonical_url_dedups_across_providers() {
        assert_eq!(canonical_url("https://www.Example.org/a/"), "example.org/a");
        assert_eq!(canonical_url("https://example.org/a"), "example.org/a");
        assert_eq!(
            canonical_url("https://example.org/a?utm_source=x"),
            "example.org/a"
        );
        assert_eq!(canonical_url("https://example.org/a#frag"), "example.org/a");
        assert_eq!(
            canonical_url("https://example.org/a?b=2&a=1"),
            "example.org/a?a=1&b=2"
        );
        assert_eq!(canonical_url("not a url"), "not a url");
    }

    #[test]
    fn merge_prefers_urls_seen_in_more_providers() {
        let a = json!({"title": "a", "url": "https://x/a", "description": "d"});
        let b = json!({"title": "b", "url": "https://x/b", "description": "d"});
        let c = json!({"title": "c", "url": "https://x/c", "description": "d"});
        let lists = vec![("wikipedia", vec![a, b.clone()]), ("crates-io", vec![b, c])];
        let fused = merge(lists, 10);
        assert_eq!(fused.len(), 3, "deduped to 3 unique URLs: {fused:?}");
        assert_eq!(
            fused[0]["url"],
            json!("https://x/b"),
            "the hit both providers returned ranks first: {fused:?}"
        );
        assert_eq!(fused[0]["sources"], json!(["crates-io", "wikipedia"]));
        assert_eq!(fused[0]["provider"], json!("wikipedia"));
    }

    #[test]
    fn merge_keeps_the_most_informative_copy_and_is_deterministic_on_ties() {
        let short = json!({"url": "https://x/a", "description": "kurz"});
        let long = json!({"url": "https://x/a/", "description": "eine viel laengere Beschreibung"});
        let fused = merge(vec![("wikipedia", vec![short]), ("mdn", vec![long])], 10);
        assert_eq!(
            fused[0]["description"],
            json!("eine viel laengere Beschreibung")
        );

        // Two different URLs with the same count *and* the same best rank must
        // come out in a stable order (by canonical URL), never in hash order —
        // even when the provider lists arrive in a different order.
        let p = json!({"url": "https://x/p", "description": ""});
        let q = json!({"url": "https://x/q", "description": ""});
        let one = merge(
            vec![("wikipedia", vec![p.clone()]), ("mdn", vec![q.clone()])],
            10,
        );
        let two = merge(vec![("mdn", vec![q]), ("wikipedia", vec![p])], 10);
        let urls = |v: Vec<Value>| -> Vec<String> {
            v.iter()
                .map(|h| h["url"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(urls(one), urls(two), "order must not depend on hash order");
    }
}
