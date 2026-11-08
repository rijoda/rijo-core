//! Turn-level transient tool retry, measured on a REAL local service.
//!
//! The proof is the request count the flaky service itself observes: a read-only
//! call that gets a transient 503 is issued once more; a deterministic 404 is
//! not. Nothing here counts internal calls — the server is the witness.
//!
//! Dedicated test binary (RIJO_LLM_MOCK is cached per process).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;

use rijo_core::config::Config;
use rijo_core::executor::Executor;

#[tokio::test]
async fn a_transient_tool_error_is_retried_once_and_a_deterministic_one_is_not() {
    // Flaky service: `flaky` answers 503 on the FIRST hit and 200 afterwards,
    // `missing` answers 404 always. `hits` counts every request that arrives.
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().route(
        "/repos/{owner}/{repo}",
        get(move |Path((_owner, repo)): Path<(String, String)>| {
            let h = h.clone();
            async move {
                let n = h.fetch_add(1, Ordering::SeqCst);
                let body = if repo == "missing" {
                    (StatusCode::NOT_FOUND, "Not Found")
                } else if n == 0 {
                    (StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable")
                } else {
                    (StatusCode::OK, "ok")
                };
                (
                    body.0,
                    axum::Json(serde_json::json!({
                        "message": body.1,
                        "full_name": "rijoda/rijo-code",
                        "private": true,
                        "default_branch": "master",
                    })),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();

    // Mock: a transient repo_view, then a deterministic-404 repo_view, then the
    // final answer.
    let repo = |name: &str| {
        serde_json::json!({
            "tool_calls": [{
                "name": "github",
                "arguments": { "action": "repo_view", "repo": format!("rijoda/{name}") }
            }]
        })
    };
    let script = dir.path().join("mock.json");
    std::fs::write(
        &script,
        serde_json::json!({ "turns": [repo("flaky"), repo("missing"), { "content": "FINAL" }] })
            .to_string(),
    )
    .unwrap();

    std::env::set_var("GITHUB_TOKEN", "test-token-not-real");
    std::env::set_var("RIJO_LLM_MOCK", &script);

    let mut config = Config::default();
    config.data_dir = dir.path().join("data");
    config.memory.enabled = false;
    config.learning.after_turn = false;
    config.router.enabled = false;
    config.context.max_iterations = 10;
    config.github.api_base = Some(format!("http://{addr}"));
    config.github.use_gh_cli = false;
    let executor = Executor::new_opts(config, false);

    let mut history = Vec::new();
    let out = executor.run_turn("do github things", &mut history).await;
    assert_eq!(out.as_deref(), Ok("FINAL"), "turn must finish: {out:?}");

    // flaky: 503 + the retry's 200 = 2 requests. missing: 404, NOT retried = 1.
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "exactly one retry for the transient 503, none for the deterministic 404"
    );
}
