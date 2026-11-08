//! Provider templates for OpenAI-compatible endpoints.
//!
//! Rijo Code speaks the OpenAI chat-completions dialect, so any compatible endpoint
//! works with `base_url` + `api_key` alone. A few providers need more than that,
//! and those extra requirements are captured here so a user does not have to
//! reverse-engineer them.
//!
//! Everything in this module was verified against the live API (2026-09-23):
//!
//! * **OpenCode Zen / Go** (`https://opencode.ai/zen/go/v1`)
//!   - the `api.opencode.ai` and `models.opencode.ai` hosts are NOT the API:
//!     they answer every request with Cloudflare error 1010 (browser-signature
//!     block), including the `/models` listing;
//!   - a session header is mandatory. Without `x-opencode-session` the request
//!     fails with HTTP 400 `MissingSessionID` — "cannot be routed efficiently";
//!   - a browser-like `User-Agent` is required, because the Cloudflare edge
//!     blocks the default reqwest agent;
//!   - streaming (SSE with a usage event), tool calls and `reasoning_content`
//!     all work like any other OpenAI-compatible endpoint.
//!
//! Credentials: this module READS the token but never writes it into Rijo Code's
//! config. The lookup order is
//!   1. `api_key` already present in the config / `RIJO_API_KEY`,
//!   2. the provider's own environment variable (`OPENCODE_API_KEY`),
//!   3. the provider's own credential file (`~/.local/share/opencode/auth.json`).
//!
//! Step 3 exists so that a machine which already has `opencode` logged in needs
//! no second copy of the key — the same pattern the GitHub tool uses with
//! `gh auth token`.

use std::path::PathBuf;

/// A ready-made provider setup.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderTemplate {
    /// Name used on the command line (`--provider <name>`).
    pub name: &'static str,
    pub label: &'static str,
    pub base_url: &'static str,
    /// Header carrying a per-process session id. Empty = none needed.
    pub session_header: &'static str,
    /// Extra headers as `Name: value` lines.
    pub extra_headers: &'static [&'static str],
    /// Environment variable the provider's own tooling uses for the token.
    pub api_key_env: &'static str,
    /// Credential file of the provider's own CLI, if it has one. A `String`
    /// rather than `&'static str` so tests can point it at a temp file.
    pub credential_file: Option<String>,
    /// JSON key inside that file holding the token.
    pub credential_key: &'static str,
    /// Model to use when the caller never chose one — see [`apply`]. Empty means
    /// the template has no useful guess.
    pub default_model: &'static str,
}

/// OpenCode Zen / Go — includes the free development models.
pub fn opencode() -> ProviderTemplate {
    ProviderTemplate {
        name: "opencode",
        label: "OpenCode (Zen/Go)",
        base_url: "https://opencode.ai/zen/go/v1",
        session_header: "x-opencode-session",
        extra_headers: &["User-Agent: opencode/1.0 (linux; x64)"],
        api_key_env: "OPENCODE_API_KEY",
        credential_file: Some("~/.local/share/opencode/auth.json".to_string()),
        credential_key: "opencode-go",
        // The free development model the README documents for this endpoint.
        // Without it a fresh `--provider opencode` would send the OpenAI default
        // model to an endpoint that does not offer it.
        default_model: "space-bunny-free",
    }
}

/// DeepSeek — the direct API (`api.deepseek.com`), not a reseller.
///
/// OpenAI-compatible, so no session header and no extra headers are needed —
/// unlike OpenCode. The thinking/reasoning handling lives in `llm.rs` and the
/// context windows (V4 = 1M, V3 = 128K) in `models.rs`; this is the piece that
/// makes `--provider deepseek` resolve the key and the endpoint.
pub fn deepseek() -> ProviderTemplate {
    ProviderTemplate {
        name: "deepseek",
        label: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        session_header: "",
        extra_headers: &[],
        api_key_env: "DEEPSEEK_API_KEY",
        // DeepSeek ships no CLI of its own, so there is no credential file to
        // read — the key comes from the environment or the config.
        credential_file: None,
        credential_key: "",
        default_model: DEEPSEEK_MODELS[0],
    }
}

/// A local llama.cpp server (Rijo Code owns the process, see `local_server.rs`).
///
/// No key and no session header: the endpoint is OpenAI-compatible on
/// `127.0.0.1`. The value of this template is not the URL but that the server is
/// long-lived, so llama-server's prefix KV cache survives across turns (measured
/// 33x less prefill warm than cold).
pub fn local() -> ProviderTemplate {
    ProviderTemplate {
        name: "local",
        label: "Local llama.cpp (persistent)",
        base_url: "http://127.0.0.1:8080/v1",
        session_header: "",
        extra_headers: &[],
        // No key is needed; an empty name means `std::env::var` finds nothing
        // and `apply` reports "token: NOT found", which is correct here.
        api_key_env: "",
        credential_file: None,
        credential_key: "",
        default_model: "local",
    }
}

/// DeepInfra — OpenAI-compatible, pay per use, no CLI of its own.
///
/// A template so the first-run surface and the CLI wizard cannot drift on the
/// URL or the default model.
pub fn deepinfra() -> ProviderTemplate {
    ProviderTemplate {
        name: "deepinfra",
        label: "DeepInfra",
        base_url: "https://api.deepinfra.com/v1/openai",
        session_header: "",
        extra_headers: &[],
        api_key_env: "DEEPINFRA_API_KEY",
        credential_file: None,
        credential_key: "",
        default_model: "Qwen/Qwen2.5-Coder-32B-Instruct",
    }
}

/// Every provider Rijo Code knows a template for.
pub fn all() -> Vec<ProviderTemplate> {
    vec![deepseek(), opencode(), local()]
}

/// Looks a template up by name (case-insensitive, `zen`/`go` accepted as
/// aliases for `opencode`).
pub fn by_name(name: &str) -> Option<ProviderTemplate> {
    let n = name.trim().to_ascii_lowercase();
    match n.as_str() {
        "deepseek" | "ds" => Some(deepseek()),
        "opencode" | "zen" | "go" | "opencode-go" => Some(opencode()),
        "deepinfra" => Some(deepinfra()),
        "local" | "llama.cpp" | "llamacpp" => Some(local()),
        _ => None,
    }
}

/// The template matching a base URL, if any. Used to warn when a config points
/// at OpenCode but is missing the headers the API insists on.
pub fn for_base(base_url: &str) -> Option<ProviderTemplate> {
    let base = base_url.trim_end_matches('/');
    if base.contains("opencode.ai/zen") {
        return Some(opencode());
    }
    if base.contains("api.deepseek.com") {
        return Some(deepseek());
    }
    if crate::local_server::is_local_base(base) {
        return Some(local());
    }
    None
}

/// Model ids the DeepSeek API documents. Documentation, not a gate: anything the
/// endpoint accepts can be passed to `--model`.
///
/// `deepseek-chat` and `deepseek-reasoner` are the two names the API lists;
/// `deepseek-reasoner` is the thinking model (`llm.rs` sends
/// `reasoning_effort` for both, see `models.rs` for the windows).
pub const DEEPSEEK_MODELS: &[&str] = &["deepseek-chat", "deepseek-reasoner"];

/// Model ids of the free tier that were listed on 2026-09-23. Documentation,
/// not a gate: anything the endpoint accepts can be passed to `--model`.
pub const OPENCODE_FREE_MODELS: &[&str] = &[
    "space-bunny-free",
    "ling-3.0-flash-fin-free",
    "mimo-v2.6-flash-free",
    "muse-spark-1.3-contributor-free",
    "nemotron-3-ultra-free",
    "nemotron-3.5-lightning-free",
];

/// Expands a leading `~/`.
fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => crate::paths::home_dir().join(rest),
        None => PathBuf::from(path),
    }
}

/// Reads the token from the provider's own credential file.
///
/// Returns `None` when the file is missing, unreadable, malformed, or holds no
/// token — never an error the caller has to handle, because a missing file is
/// the normal case on a machine without that CLI installed.
pub fn token_from_credential_file(tpl: &ProviderTemplate) -> Option<String> {
    let path = expand_home(tpl.credential_file.as_deref()?);
    let content = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    // Preferred key first, then any entry that carries a token — opencode stores
    // one object per provider and the exact name has changed between releases.
    let entry = json
        .get(tpl.credential_key)
        .and_then(|v| v.get("key").or_else(|| v.get("api_key")))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            json.as_object()?
                .values()
                .filter_map(|v| v.get("key").and_then(|k| k.as_str()))
                .find(|s| !s.trim().is_empty())
                .map(|s| s.to_string())
        })?;

    Some(entry)
}

/// Resolves the token for a provider: environment variable first, then the
/// provider's own credential file. `configured` is the value already in the
/// config — if it is non-empty the caller does not need this at all.
pub fn resolve_token(tpl: &ProviderTemplate, configured: &str) -> Option<String> {
    if !configured.trim().is_empty() {
        return None;
    }
    if let Ok(v) = std::env::var(tpl.api_key_env) {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    token_from_credential_file(tpl)
}

/// Applies a template to a config: base URL and the headers the API requires.
///
/// The token is resolved as follows:
/// * a key in the config counts only when it belongs to *this* provider — i.e.
///   the config already pointed at the template's base URL. A key for a
///   different endpoint (the usual case: DeepSeek in `config.yaml`, now
///   switching to OpenCode) is NOT reused; reusing it produced a real HTTP 401
///   `AuthError: Invalid API key` before this rule existed.
/// * otherwise the provider's own environment variable, then its credential file.
///
/// The resolved token stays in memory for this run; this function never writes
/// it to disk.
///
/// Returns a short note for the caller to print (never the secret itself).
pub fn apply(cfg: &mut crate::config::LlmConfig, tpl: &ProviderTemplate) -> String {
    let previous_base = cfg.base_url.clone();
    let same_provider = for_base(&previous_base)
        .map(|p| p.base_url == tpl.base_url)
        .unwrap_or(false);

    cfg.base_url = tpl.base_url.to_string();
    cfg.session_header = tpl.session_header.to_string();
    cfg.extra_headers = tpl.extra_headers.iter().map(|s| s.to_string()).collect();

    // Model: take the template's default only when the caller never chose one.
    // The built-in default is an OpenAI model and cannot work against another
    // endpoint, so leaving it there is a guaranteed rejection — measured:
    // `--provider deepseek` without `--model` sent `gpt-4o-mini` to
    // api.deepseek.com. A model the user set is never touched.
    let untouched_model =
        cfg.model.trim().is_empty() || cfg.model == crate::config::Config::default().llm.model;
    if untouched_model && !tpl.default_model.is_empty() {
        cfg.model = tpl.default_model.to_string();
    }

    if same_provider && !cfg.api_key.trim().is_empty() {
        return "token: from the config".to_string();
    }

    if let Ok(v) = std::env::var(tpl.api_key_env) {
        if !v.trim().is_empty() {
            cfg.api_key = v;
            return format!("token: from ${}", tpl.api_key_env);
        }
    }
    if let Some(token) = token_from_credential_file(tpl) {
        cfg.api_key = token;
        return format!(
            "token: from {}",
            tpl.credential_file.as_deref().unwrap_or("(unknown)")
        );
    }
    if !cfg.api_key.trim().is_empty() {
        // A key for a DIFFERENT provider must not be reused — reusing it
        // produced a real HTTP 401 before this rule existed. Clear it so the
        // caller never sends a foreign key to this provider.
        cfg.api_key.clear();
        return "token: NOT found — set the env var or log in with the provider's CLI".to_string();
    }
    "token: NOT found — set the env var or log in with the provider's CLI".to_string()
}

/// Multi-line description for `--list-providers`. No API call, no secret.
pub fn describe(tpl: &ProviderTemplate) -> String {
    let mut s = String::new();
    s.push_str(&format!("{}  (--provider {})\n", tpl.label, tpl.name));
    s.push_str(&format!("  base_url        {}\n", tpl.base_url));
    if !tpl.session_header.is_empty() {
        s.push_str(&format!("  session_header  {}\n", tpl.session_header));
    }
    for h in tpl.extra_headers {
        s.push_str(&format!("  extra_header    {h}\n"));
    }
    s.push_str(&format!("  token env       ${}\n", tpl.api_key_env));
    if let Some(f) = tpl.credential_file.as_deref() {
        s.push_str(&format!("  credential file {f}\n"));
    }
    s.push_str(&format!(
        "  token           {}\n",
        match resolve_token(tpl, "") {
            Some(_) => "available",
            None => "NOT available (set the env var or log in with the provider's CLI)",
        }
    ));
    if tpl.name == "opencode" {
        s.push_str("  free models     ");
        s.push_str(&OPENCODE_FREE_MODELS.join(", "));
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_opencode_template_carries_what_the_api_requires() {
        let oc = opencode();
        assert_eq!(oc.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(oc.session_header, "x-opencode-session");
        // The Cloudflare edge rejects the default agent (error 1010).
        assert!(
            oc.extra_headers
                .iter()
                .any(|h| h.starts_with("User-Agent:")),
            "{:?}",
            oc.extra_headers
        );
        assert_eq!(oc.api_key_env, "OPENCODE_API_KEY");
    }

    #[test]
    fn the_free_model_list_is_not_empty() {
        assert!(OPENCODE_FREE_MODELS.contains(&"space-bunny-free"));
    }

    #[test]
    fn lookup_accepts_the_aliases_and_rejects_the_rest() {
        for alias in ["opencode", "OpenCode", "zen", "go", "opencode-go"] {
            assert!(by_name(alias).is_some(), "alias {alias} not recognised");
        }
        for alias in ["deepseek", "DeepSeek", "ds"] {
            assert!(by_name(alias).is_some(), "alias {alias} not recognised");
        }
        assert!(by_name("").is_none());
        assert!(by_name("no-such-provider").is_none());
    }

    /// The direct DeepSeek API: no session header, no extra headers, and the key
    /// comes from `DEEPSEEK_API_KEY` — DeepSeek ships no CLI, so there is no
    /// credential file to fall back to.
    #[test]
    fn deepseek_is_a_first_class_provider() {
        let ds = by_name("deepseek").expect("deepseek template");
        assert_eq!(ds.base_url, "https://api.deepseek.com/v1");
        assert_eq!(ds.api_key_env, "DEEPSEEK_API_KEY");
        assert!(
            ds.session_header.is_empty(),
            "DeepSeek needs no session header"
        );
        assert!(
            ds.extra_headers.is_empty(),
            "DeepSeek needs no extra headers"
        );
        assert!(
            ds.credential_file.is_none(),
            "DeepSeek has no CLI credential file"
        );
        // And it is part of the advertised set, so --list-providers shows it.
        assert!(all().iter().any(|t| t.name == "deepseek"));
    }

    #[test]
    fn deepseek_key_resolution_reads_the_environment() {
        let ds = by_name("deepseek").unwrap();
        // A configured key always wins (resolve_token returns None = "keep it").
        assert_eq!(resolve_token(&ds, "already-set"), None);
        // Otherwise the provider's own variable is used.
        std::env::set_var("DEEPSEEK_API_KEY", "ds-test-key");
        assert_eq!(resolve_token(&ds, "").as_deref(), Some("ds-test-key"));
        // An empty variable counts as unset, not as "the key is empty".
        std::env::set_var("DEEPSEEK_API_KEY", "   ");
        assert_eq!(resolve_token(&ds, ""), None);
        std::env::remove_var("DEEPSEEK_API_KEY");
    }

    #[test]
    fn detection_by_base_url_has_no_false_positives() {
        assert!(for_base("https://opencode.ai/zen/go/v1/").is_some());
        assert!(for_base("https://api.deepseek.com/v1").is_some());
        assert!(for_base("https://api.openai.com/v1").is_none());
    }

    #[test]
    fn a_configured_key_is_never_replaced() {
        // Returning None means "the caller's key stands" — the point of the
        // check is that a set key is not silently overwritten by a file.
        let oc = opencode();
        assert_eq!(resolve_token(&oc, "already-set"), None);
    }

    #[test]
    fn a_key_for_another_provider_is_not_reused() {
        // The real failure this guards against: config.yaml holds the DeepSeek
        // key, `--provider opencode` switches the base URL, and the old key is
        // sent to OpenCode -> HTTP 401 AuthError: Invalid API key.
        let oc = opencode();
        let mut cfg = crate::config::LlmConfig {
            base_url: "https://api.deepseek.com/v1".into(),
            api_key: "deepseek-key".into(),
            ..crate::config::Config::default().llm
        };
        std::env::remove_var("OPENCODE_API_KEY");
        let note = apply(&mut cfg, &oc);
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.session_header, "x-opencode-session");
        assert!(!cfg.extra_headers.is_empty());
        // Either the provider's own credential file supplied a token, or none was
        // found — never the foreign key.
        assert_ne!(cfg.api_key, "deepseek-key", "note was: {note}");
        assert!(
            note.contains("auth.json") || note.starts_with("token: NOT found"),
            "note was: {note}"
        );
    }

    #[test]
    fn a_key_of_the_same_provider_is_kept() {
        let oc = opencode();
        let mut cfg = crate::config::LlmConfig {
            base_url: "https://opencode.ai/zen/go/v1".into(),
            api_key: "own-key".into(),
            ..crate::config::Config::default().llm
        };
        let note = apply(&mut cfg, &oc);
        assert_eq!(cfg.api_key, "own-key");
        assert_eq!(note, "token: from the config");
    }

    #[test]
    fn the_description_never_contains_a_token() {
        let oc = opencode();
        let text = describe(&oc);
        assert!(text.contains("opencode.ai/zen/go/v1"));
        if let Some(token) = token_from_credential_file(&oc) {
            assert!(
                !text.contains(&token),
                "the token leaked into --list-providers"
            );
        }
    }

    #[test]
    fn the_environment_variable_beats_the_credential_file() {
        let oc = opencode();
        // Unique variable name per test run is not needed: the value is only
        // compared, and no other test in this binary touches this variable.
        std::env::set_var("OPENCODE_API_KEY", "from-env");
        assert_eq!(resolve_token(&oc, "").as_deref(), Some("from-env"));
        std::env::remove_var("OPENCODE_API_KEY");
    }

    #[test]
    fn a_missing_credential_file_is_not_an_error() {
        let tpl = ProviderTemplate {
            credential_file: Some("~/.rijo/definitely-not-here.json".to_string()),
            ..opencode()
        };
        assert_eq!(token_from_credential_file(&tpl), None);
    }

    #[test]
    fn a_malformed_credential_file_is_not_an_error() {
        let dir = std::env::temp_dir().join("rijo_prov_test_malformed");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("auth.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        let tpl = ProviderTemplate {
            credential_file: Some(path.display().to_string()),
            ..opencode()
        };
        assert_eq!(token_from_credential_file(&tpl), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_token_is_read_from_the_named_entry() {
        let dir = std::env::temp_dir().join("rijo_prov_test_named");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{"other":{"key":"wrong"},"opencode-go":{"type":"api","key":"right"}}"#,
        )
        .unwrap();
        let tpl = ProviderTemplate {
            credential_file: Some(path.display().to_string()),
            ..opencode()
        };
        assert_eq!(token_from_credential_file(&tpl).as_deref(), Some("right"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unnamed_entry_is_still_used_as_a_fallback() {
        let dir = std::env::temp_dir().join("rijo_prov_test_fallback");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("auth.json");
        std::fs::write(&path, r#"{"renamed-provider":{"key":"fallback"}}"#).unwrap();
        let tpl = ProviderTemplate {
            credential_file: Some(path.display().to_string()),
            ..opencode()
        };
        assert_eq!(
            token_from_credential_file(&tpl).as_deref(),
            Some("fallback")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_token_counts_as_missing() {
        let dir = std::env::temp_dir().join("rijo_prov_test_empty");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("auth.json");
        std::fs::write(&path, r#"{"opencode-go":{"key":"   "}}"#).unwrap();
        let tpl = ProviderTemplate {
            credential_file: Some(path.display().to_string()),
            ..opencode()
        };
        assert_eq!(token_from_credential_file(&tpl), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
