//! Context compression wired into the agent loop: when the request nears the
//! model window, the older history is folded into one summary so a long, broad
//! task does not overflow the context.
//!
//! Dedicated test binary (RIJO_LLM_MOCK is cached per process).

use rijo_core::config::Config;
use rijo_core::executor::Executor;

#[tokio::test]
async fn the_loop_folds_older_history_into_a_summary() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();

    // idx0, idx1: two distinct tool turns. idx2: the compression summary call.
    // idx3: the final answer. If compression did NOT run, the main call at
    // iteration 3 would receive idx2 ("SUMMARY TEXT") as its answer — so seeing
    // "FINAL" proves the summary call consumed idx2.
    let mut turns: Vec<serde_json::Value> = Vec::new();
    for i in 0..2 {
        let p = dir.path().join(format!("f{i}.txt"));
        turns.push(serde_json::json!({
            "tool_calls": [{ "name": "read_file", "arguments": { "path": p.to_string_lossy() } }]
        }));
    }
    turns.push(serde_json::json!({ "content": "SUMMARY TEXT" }));
    turns.push(serde_json::json!({ "content": "FINAL" }));
    let script = dir.path().join("mock.json");
    std::fs::write(&script, serde_json::json!({ "turns": turns }).to_string()).unwrap();
    std::env::set_var("RIJO_LLM_MOCK", &script);

    let mut config = Config::default();
    config.data_dir = dir.path().join("data");
    config.memory.enabled = false;
    config.learning.after_turn = false;
    config.router.enabled = false;
    config.context.max_iterations = 10;
    // A small window and a low threshold so the trigger is reached after the
    // minimum gap, without a huge fixture.
    config.llm.context_window = 1_000;
    config.context.compress_at_pct = 1; // ≥1 % of 1000 ≈ 10 tokens
    config.context.min_turns_between = 2;
    config.context.keep_recent_messages = 2;
    config.context.min_saving_tokens = 0;
    config.context.summary_max_tokens = 100;
    let executor = Executor::new_opts(config, false);

    let mut history = Vec::new();
    let out = executor.run_turn("do a long thing", &mut history).await;

    assert_eq!(
        out.as_deref(),
        Ok("FINAL"),
        "compression must consume the summary turn (else the answer would be \
         \"SUMMARY TEXT\"): {out:?}"
    );
}
