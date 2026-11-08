//! The LLM half of context compression.
//!
//! A dedicated test binary on purpose: `RIJO_LLM_MOCK` is cached per process, so
//! this process must be the only one that sets it (a fresh binary guarantees
//! that). What is proven is the harness — the prompt goes out, the answer is
//! parsed and capped — not the model.

use rijo_core::compress::{message, summarize};
use rijo_core::config::LlmConfig;
use rijo_core::llm::LlmClient;

fn test_llm() -> LlmClient {
    // The mock intercepts before any network call, so the endpoint is irrelevant.
    LlmClient::new(&LlmConfig {
        base_url: "http://127.0.0.1:1/v1".into(),
        api_key: "test".into(),
        model: "test".into(),
        max_tokens: 1024,
        temperature: 0.0,
        thinking_effort: "med".into(),
        context_window: 0,
        session_header: String::new(),
        extra_headers: Vec::new(),
    })
}

#[tokio::test]
async fn summarize_replays_the_scripted_answer() {
    let path = std::env::temp_dir().join(format!("rijo_compress_mock_{}.json", std::process::id()));
    std::fs::write(
        &path,
        r#"{"turns":[{"content":"folded: crates/rijo-core, watermark 42"}]}"#,
    )
    .expect("write mock");
    std::env::set_var("RIJO_LLM_MOCK", &path);

    let llm = test_llm();
    let msgs = vec![
        message("user", "where is the registry"),
        message("assistant", "crates/rijo-core/src/tools/mod.rs"),
    ];
    let out = summarize(&llm, Some("earlier facts"), &msgs, 1200)
        .await
        .expect("summarize");
    assert_eq!(out, "folded: crates/rijo-core, watermark 42");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn summarize_refuses_an_empty_block() {
    // No LLM call needed: an empty input is rejected before the model.
    let llm = test_llm();
    let err = summarize(&llm, None, &[], 1200).await.unwrap_err();
    assert!(err.contains("nothing to summarize"), "{err}");
}
