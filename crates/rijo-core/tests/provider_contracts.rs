//! Integration contracts for provider templates (`--provider` / `--model`).
//!
//! These run without a network call. The live behaviour was verified by hand
//! against `https://opencode.ai/zen/go/v1` (2026-09-23): a plain request without
//! the session header is rejected with HTTP 400 `MissingSessionID`, and the
//! default reqwest agent is blocked by the Cloudflare edge (error 1010). Both
//! facts are what the template encodes, so both are asserted here.

use clap::Parser;
use rijo_core::providers;
use serde_json::json;

/// Parses arguments the way the binary does, without a real process.
fn parse(args: &[&str]) -> rijo_core::cli::Cli {
    let mut full = vec!["rijo"];
    full.extend_from_slice(args);
    rijo_core::cli::Cli::try_parse_from(full).expect("args must parse")
}

/// Serializes the tests that touch a process-wide provider variable.
///
/// `apply` reads the environment, and the test binary runs its tests in
/// parallel: without this, one test's `remove_var` lands between another's
/// `set_var` and its assertion, and the failure looks like a code bug (measured:
/// "token: NOT found" while the key was set).
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn the_cli_exposes_the_provider_flags() {
    let cli = parse(&["--provider", "opencode", "--model", "space-bunny-free"]);
    assert_eq!(cli.provider.as_deref(), Some("opencode"));
    assert_eq!(cli.model.as_deref(), Some("space-bunny-free"));

    // The base URL override exists for endpoints without a template.
    let cli = parse(&["--base-url", "https://example.test/v1"]);
    assert_eq!(cli.base_url.as_deref(), Some("https://example.test/v1"));

    // Listing is a flag, not an argument.
    assert!(parse(&["--list-providers"]).list_providers);

    // Default: no provider, so the config decides.
    let cli = parse(&[]);
    assert!(cli.provider.is_none() && cli.model.is_none());
}

#[test]
fn the_local_provider_template_needs_no_key() {
    let tpl = providers::by_name("local").expect("local template");
    assert_eq!(tpl.base_url, "http://127.0.0.1:8080/v1");
    assert!(
        tpl.api_key_env.is_empty(),
        "a local server needs no key env var"
    );
    assert!(tpl.session_header.is_empty());

    // A localhost base URL resolves to the local template (so a config pointing
    // at a running llama-server is recognised), while DeepSeek stays DeepSeek.
    assert_eq!(
        providers::for_base("http://127.0.0.1:11434/v1").map(|t| t.name),
        Some("local")
    );
    assert_eq!(
        providers::for_base("https://api.deepseek.com/v1").map(|t| t.name),
        Some("deepseek")
    );
}

#[test]
fn an_unknown_provider_name_is_rejected_by_lookup() {
    assert!(providers::by_name("not-a-provider").is_none());
    // ...but the CLI itself accepts any string, so the error can name the
    // known providers instead of a bare clap message.
    assert_eq!(
        parse(&["--provider", "not-a-provider"]).provider.as_deref(),
        Some("not-a-provider")
    );
}

#[test]
fn applying_the_template_sets_everything_the_api_demands() {
    let mut llm = rijo_core::config::Config::default().llm;
    llm.base_url = "https://api.deepseek.com/v1".into();
    llm.api_key = "key-of-another-provider".into();

    let tpl = providers::by_name("opencode").expect("opencode template");
    let note = providers::apply(&mut llm, &tpl);

    assert_eq!(llm.base_url, "https://opencode.ai/zen/go/v1");
    assert_eq!(llm.session_header, "x-opencode-session");
    assert_eq!(
        llm.extra_headers,
        vec!["User-Agent: opencode/1.0 (linux; x64)".to_string()]
    );
    // The foreign key must not survive: it caused a real 401 in a live run.
    assert_ne!(llm.api_key, "key-of-another-provider");
    assert!(
        note.contains("auth.json") || note.starts_with("token: NOT found"),
        "note was: {note}"
    );
}

#[test]
fn the_note_and_the_listing_never_leak_the_token() {
    let tpl = providers::by_name("opencode").unwrap();

    let mut llm = rijo_core::config::Config::default().llm;
    llm.api_key.clear();
    let note = providers::apply(&mut llm, &tpl);
    let token = llm.api_key.clone();

    if !token.trim().is_empty() {
        assert!(!note.contains(&token), "the note leaked the token: {note}");
        assert!(
            !providers::describe(&tpl).contains(&token),
            "the listing leaked the token"
        );
    }
}

#[test]
fn the_listing_documents_the_free_models_and_the_endpoint() {
    let text = providers::describe(&providers::by_name("opencode").unwrap());
    assert!(text.contains("https://opencode.ai/zen/go/v1"));
    assert!(text.contains("x-opencode-session"));
    assert!(text.contains("space-bunny-free"));
    assert!(text.contains("$OPENCODE_API_KEY"));
}

#[test]
fn a_bare_opencode_base_url_still_gets_the_required_headers() {
    // `--base-url https://opencode.ai/zen/go/v1` without `--provider` must not
    // produce a request that the API answers with MissingSessionID.
    let tpl = providers::for_base("https://opencode.ai/zen/go/v1");
    assert!(tpl.is_some(), "the base URL must be recognised");
    let tpl = tpl.unwrap();
    assert_eq!(tpl.session_header, "x-opencode-session");
    assert!(!tpl.extra_headers.is_empty());

    // And a different endpoint is left alone.
    assert!(providers::for_base("https://api.openai.com/v1").is_none());
    assert!(providers::for_base("https://no-such-endpoint.example/v1").is_none());
}

/// The acceptance path for "usable via the DeepSeek API": `--provider deepseek`
/// must point at the official endpoint and take the key from
/// `DEEPSEEK_API_KEY`, with no session header (DeepSeek needs none).
#[test]
fn the_deepseek_provider_is_ready_without_any_config_editing() {
    let _guard = env_lock();
    let tpl = providers::by_name("deepseek").expect("deepseek template");
    let mut llm = rijo_core::config::Config::default().llm;
    llm.base_url = "https://api.openai.com/v1".into();
    llm.api_key = "gpt-key".into();

    std::env::set_var("DEEPSEEK_API_KEY", "ds-key");
    let note = providers::apply(&mut llm, &tpl);

    assert_eq!(llm.base_url, "https://api.deepseek.com/v1");
    // The untouched OpenAI default model is replaced: it cannot work here.
    assert_eq!(llm.model, "deepseek-chat");
    // The foreign OpenAI key is dropped, the DeepSeek one is picked up.
    assert_eq!(llm.api_key, "ds-key", "note was: {note}");
    assert!(note.contains("DEEPSEEK_API_KEY"), "note was: {note}");
    // DeepSeek needs neither a session header nor extra headers.
    assert!(llm.session_header.is_empty());
    assert!(llm.extra_headers.is_empty());

    // A bare --base-url must be recognised too, so the config path works.
    let by_base = providers::for_base("https://api.deepseek.com/v1");
    assert_eq!(by_base.map(|t| t.name), Some("deepseek"));

    std::env::remove_var("DEEPSEEK_API_KEY");
}

/// The other half of the model rule: a model the user chose is never replaced.
#[test]
fn a_chosen_model_survives_applying_a_template() {
    let _guard = env_lock();
    let tpl = providers::by_name("deepseek").expect("deepseek template");
    let mut llm = rijo_core::config::Config::default().llm;
    llm.model = "deepseek-reasoner".into();
    std::env::set_var("DEEPSEEK_API_KEY", "ds-key");
    providers::apply(&mut llm, &tpl);
    assert_eq!(llm.model, "deepseek-reasoner", "a chosen model must stay");
    std::env::remove_var("DEEPSEEK_API_KEY");
}

#[test]
fn header_lines_are_parsed_defensively() {
    // A config file is typed by hand, so a broken line must be dropped, not
    // panic — and never turn into an empty header name.
    let parsed = rijo_core::llm::parse_header_lines(&[
        "X-Good: value".to_string(),
        "no-colon-here".to_string(),
        ": empty-name".to_string(),
        "X-Blank:   ".to_string(),
        "  X-Spaced  :  v  ".to_string(),
    ]);
    assert_eq!(
        parsed,
        vec![
            ("X-Good".to_string(), "value".to_string()),
            ("X-Spaced".to_string(), "v".to_string()),
        ]
    );

    // The template's own header line survives parsing.
    let tpl = providers::by_name("opencode").unwrap();
    let lines: Vec<String> = tpl.extra_headers.iter().map(|s| s.to_string()).collect();
    let parsed = rijo_core::llm::parse_header_lines(&lines);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].0, "User-Agent");
    assert!(parsed[0].1.contains("opencode"));
}

#[test]
fn the_provider_schema_is_free_of_credentials_in_the_tool_registry() {
    // The provider switch is a CLI flag, not a tool — the model must not be able
    // to change its own endpoint or read the token through a tool schema.
    let registry = rijo_core::tools::Registry::build_default();
    let names: Vec<&str> = registry.get_names();
    assert!(!names.iter().any(|n| n.contains("provider")), "{names:?}");

    for name in &names {
        if let Some(schema) = registry.get_schema(name) {
            let text = serde_json::to_string(&json!(schema.raw())).unwrap();
            assert!(
                !text.to_lowercase().contains("api_key"),
                "{name} exposes api_key"
            );
        }
    }
}
