//! Agent-loop behaviour under the offline mock: the loop must always end with an
//! **answer**, never a bare error — a broad task spectra must still produce a
//! result when the iteration cap is hit.
//!
//! A dedicated test binary on purpose: `RIJO_LLM_MOCK` is cached per process, so
//! this binary must be the only one that sets it (same reason as
//! `compress_contracts.rs`).

use rijo_core::config::Config;
use rijo_core::executor::Executor;

#[tokio::test]
async fn a_long_tool_loop_ends_with_a_final_answer_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();

    // Three distinct read-only tool turns (distinct args → no repetition guard),
    // then the answer the wrap-up is expected to yield.
    let mut turns: Vec<serde_json::Value> = Vec::new();
    for i in 0..3 {
        let p = dir.path().join(format!("f{i}.txt"));
        turns.push(serde_json::json!({
            "tool_calls": [{ "name": "read_file", "arguments": { "path": p.to_string_lossy() } }]
        }));
    }
    turns.push(serde_json::json!({ "content": "FINAL SUMMARY" }));
    let script = dir.path().join("mock.json");
    std::fs::write(&script, serde_json::json!({ "turns": turns }).to_string()).unwrap();
    std::env::set_var("RIJO_LLM_MOCK", &script);

    let mut config = Config::default();
    config.data_dir = dir.path().join("data");
    config.memory.enabled = false;
    config.learning.after_turn = false;
    config.router.enabled = false;
    // Small cap so the wrap-up is reached right after the 3 tool turns.
    config.context.max_iterations = 3;
    let executor = Executor::new_opts(config, false);

    let mut history = Vec::new();
    let out = executor.run_turn("do a long thing", &mut history).await;

    assert_eq!(
        out.as_deref(),
        Ok("FINAL SUMMARY"),
        "the loop must wrap up with an answer, not error: {out:?}"
    );
}
