//! Turn-local read-only reuse: an identical read-only call repeated in the same
//! turn is served from the turn cache instead of being dispatched again.
//!
//! Dedicated test binary (RIJO_LLM_MOCK is cached per process).

use rijo_core::config::Config;
use rijo_core::executor::{Executor, ProgressEvent};

#[tokio::test]
async fn an_identical_read_only_call_is_served_from_the_turn_cache() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    let file = dir.path().join("f.txt");
    std::fs::write(&file, "hello").unwrap();

    // Two identical read_file calls, then the answer. The 2nd must NOT be
    // dispatched — it is the turn cache's job; the repetition guard only kicks in
    // on the 3rd, so this is the case that actually reaches the tool today.
    let read = serde_json::json!({
        "tool_calls": [{ "name": "read_file", "arguments": { "path": file.to_string_lossy() } }]
    });
    let script = dir.path().join("mock.json");
    std::fs::write(
        &script,
        serde_json::json!({ "turns": [read.clone(), read.clone(), { "content": "DONE" }] })
            .to_string(),
    )
    .unwrap();
    std::env::set_var("RIJO_LLM_MOCK", &script);

    let mut config = Config::default();
    config.data_dir = dir.path().join("data");
    config.memory.enabled = false;
    config.learning.after_turn = false;
    config.router.enabled = false;
    config.context.max_iterations = 10;
    let executor = Executor::new_opts(config, false);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut history = Vec::new();
    let out = executor
        .run_turn_with_progress("read it twice", &mut history, Some(tx))
        .await;
    assert_eq!(out.as_deref(), Ok("DONE"), "turn must finish: {out:?}");

    let mut reads: Vec<(u64, String)> = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let ProgressEvent::ToolDone {
            tool,
            duration_ms,
            excerpt,
            ..
        } = ev
        {
            if tool == "read_file" {
                reads.push((duration_ms, excerpt));
            }
        }
    }
    assert_eq!(reads.len(), 2, "both read calls are reported: {reads:?}");
    assert!(
        reads[0].1.contains("hello"),
        "first read is the real dispatch: {}",
        reads[0].1
    );
    assert_eq!(reads[1].0, 0, "second read must cost no dispatch time");
    assert!(
        reads[1].1.contains("REUSED"),
        "second read is served from the cache: {}",
        reads[1].1
    );
}
