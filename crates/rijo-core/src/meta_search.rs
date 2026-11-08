//! Built-in, keyless meta-search — "welded into" Rijo Code.
//!
//! Implements the lightweight part of what a SearXNG instance does, but with no
//! external service and no Python: query an engine directly and parse the
//! result. Today: **DuckDuckGo Lite** (`lite.duckduckgo.com/lite/`), whose HTML
//! is small and stable (`a.result-link` + `td.result-snippet`, the real URL in
//! the `uddg` query parameter).
//!
//! Design constraints (docs/web-search-plan.md, P3):
//! - **Lightweight**: no new dependency; parsing is a couple of regexes.
//! - **Efficient**: one GET, no JS, no captcha handling.
//! - **Honest + fail-open**: any change to the page yields `Ok(vec![])` (the
//!   caller falls through to the next provider) — never a wrong result.
//! - **Polite**: the caller owns the shared pooled client, the per-provider
//!   timeout and the cooldown; this module only parses.
//!
//! Mojeek was evaluated and rejected: it answers with a JavaScript captcha
//! (`altcha`) to non-browser clients, so it cannot be scraped reliably.

use regex::Regex;
use serde_json::{json, Value};
use std::time::Duration;

/// The engine name reported in a hit's `provider` field.
pub const PROVIDER: &str = "duckduckgo";

/// The public DuckDuckGo Lite endpoint (overridable via `web.ddg_api`).
pub const DDG_ENDPOINT: &str = "https://lite.duckduckgo.com/lite/";

/// Query DuckDuckGo Lite and return normalized hits (`title`/`url`/`description`).
///
/// `base` is the endpoint (default [`DDG_ENDPOINT`]); tests point it at a local
/// server. `Err` is a transport error (network/timeout/HTTP status); a page whose
/// shape changed returns `Ok(vec![])` so the cascade simply moves on.
pub async fn duckduckgo(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<Value>, String> {
    let q: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let sep = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{sep}q={q}");
    let resp = client
        .get(&url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    // Only HTTP 200 is a real answer page. DuckDuckGo answers a 202 with its
    // anti-bot shell from a blocked network (measured 6/6) — that is a failure
    // and must be *named* in the note, not silently parsed into zero hits.
    if resp.status() != reqwest::StatusCode::OK {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let html = resp.text().await.map_err(|e| e.to_string())?;
    Ok(parse_ddg_lite(&html, limit))
}

/// Parse a DuckDuckGo Lite results page. Kept separate so it is unit-testable
/// without the network.
pub fn parse_ddg_lite(html: &str, limit: usize) -> Vec<Value> {
    // `<a rel="nofollow" href="…" class='result-link'>Title</a>`
    let link_re =
        Regex::new(r#"(?is)href="([^"]+)"\s+class='result-link'>(.*?)</a>"#).expect("static regex");
    // `<td class='result-snippet'> … </td>`
    let snip_re =
        Regex::new(r#"(?is)<td class='result-snippet'>(.*?)</td>"#).expect("static regex");
    let tag_re = Regex::new(r"(?is)<[^>]*>").expect("static regex");

    let links: Vec<(String, String)> = link_re
        .captures_iter(html)
        .filter_map(|c| {
            let href = c.get(1)?.as_str();
            let url = ddg_real_url(href);
            if url.is_empty() {
                return None;
            }
            let title = decode_entities(&tag_re.replace_all(c.get(2)?.as_str(), ""));
            Some((title.trim().to_string(), url))
        })
        .collect();

    // Snippets appear in the same order as the links (one `td` per result).
    let snippets: Vec<String> = snip_re
        .captures_iter(html)
        .map(|c| {
            decode_entities(&tag_re.replace_all(&c[1], " "))
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();

    links
        .into_iter()
        .enumerate()
        .take(limit)
        .map(|(i, (title, url))| {
            json!({
                "title": title,
                "url": url,
                "description": snippets.get(i).cloned().unwrap_or_default(),
            })
        })
        .collect()
}

/// A DDG Lite link is a redirect (`//duckduckgo.com/l/?uddg=<encoded>`); the real
/// URL is the `uddg` parameter. Anything else is returned unchanged.
fn ddg_real_url(href: &str) -> String {
    let absolute = if let Some(rest) = href.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        href.to_string()
    };
    if let Ok(u) = url::Url::parse(&absolute) {
        if let Some((_, target)) = u.query_pairs().find(|(k, _)| k == "uddg") {
            return target.into_owned();
        }
    }
    absolute
}

/// Decode the handful of HTML entities that appear in DDG output.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find('&') {
        out.push_str(&rest[..idx]);
        rest = &rest[idx..];
        let semi = rest.find(';').filter(|end| *end <= 8);
        match semi {
            Some(end) => {
                let entity = &rest[1..end];
                out.push_str(&decode_one(entity).unwrap_or_else(|| rest[..=end].to_string()));
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_one(entity: &str) -> Option<String> {
    let named = match entity {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => " ",
        "hellip" => "…",
        _ => {
            // Numeric: &#39; or &#x27;
            let code = if let Some(hex) = entity.strip_prefix("#x").or(entity.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok()
            } else if let Some(dec) = entity.strip_prefix('#') {
                dec.parse::<u32>().ok()
            } else {
                None
            };
            return code.and_then(char::from_u32).map(|c| c.to_string());
        }
    };
    Some(named.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"<table><tr><td>1.</td><td>
      <a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&amp;rut=abc" class='result-link'>Rust Programming <b>Language</b></a>
    </td></tr>
    <tr><td></td><td class='result-snippet'><b>Rust</b> is a fast, reliable language. It&#x27;s great &amp; safe.</td></tr>
    <tr><td>2.</td><td>
      <a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FRust_(programming_language)&amp;rut=def" class='result-link'>Rust (programming language) - Wikipedia</a>
    </td></tr>
    <tr><td></td><td class='result-snippet'>Rust is a general-purpose language.</td></tr></table>"#;

    #[test]
    fn parses_titles_urls_and_snippets() {
        let hits = parse_ddg_lite(FIXTURE, 10);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0]["title"], json!("Rust Programming Language"));
        // The redirect is unwrapped to the real URL.
        assert_eq!(hits[0]["url"], json!("https://rust-lang.org/"));
        assert_eq!(
            hits[0]["description"],
            json!("Rust is a fast, reliable language. It's great & safe.")
        );
        assert_eq!(
            hits[1]["url"],
            json!("https://en.wikipedia.org/wiki/Rust_(programming_language)")
        );
    }

    #[test]
    fn limit_is_respected() {
        assert_eq!(parse_ddg_lite(FIXTURE, 1).len(), 1);
    }

    #[test]
    fn a_changed_page_yields_no_hits_instead_of_garbage() {
        assert!(parse_ddg_lite("<html><body>nothing here</body></html>", 10).is_empty());
    }
}
