//! Rijo Code — main entry point (thin tip crate for subsecond hot-patching)

use rijo_core::{cli::Cli, config::Config};

mod mcp;

#[tokio::main]
async fn main() {
    // subsecond unwinds to this call when a hot patch lands
    match subsecond::call(main_inner).await {
        Ok(()) => {}
        Err(e) => {
            eprintln!("Rijo Code error: {e}");
            // restart after a patch-forced unwind? For now: exit cleanly
            std::process::exit(1);
        }
    }
}

async fn main_inner() -> Result<(), String> {
    let cli = Cli::parse_args();

    let mut config = Config::load(cli.config.clone());

    // Provider template (--provider) and model override (--model) for this run.
    // Applied before onboarding so a first start can already target another
    // endpoint. The token is read from the provider's own env var or credential
    // file and kept in memory — it is never written into the config file.
    if let Some(name) = cli.provider.as_deref() {
        match rijo_core::providers::by_name(name) {
            Some(tpl) => {
                let note = rijo_core::providers::apply(&mut config.llm, &tpl);
                eprintln!("  provider: {} ({})", tpl.label, note);
            }
            None => {
                let known: Vec<&str> = rijo_core::providers::all().iter().map(|p| p.name).collect();
                eprintln!(
                    "  provider: unknown '{}' — known: {}",
                    name,
                    known.join(", ")
                );
                eprintln!("  see `rijo --list-providers`");
                std::process::exit(2);
            }
        }
    }
    if let Some(base) = cli.base_url.as_deref() {
        config.llm.base_url = base.to_string();
        // A bare endpoint that happens to be OpenCode still needs its headers,
        // otherwise every request comes back as HTTP 400 MissingSessionID.
        if let Some(tpl) = rijo_core::providers::for_base(base) {
            if config.llm.session_header.trim().is_empty() {
                let note = rijo_core::providers::apply(&mut config.llm, &tpl);
                eprintln!("  provider: {} ({})", tpl.label, note);
            }
        }
    }
    if let Some(model) = cli.model.as_deref() {
        config.llm.model = model.to_string();
    }

    // `--local <model.gguf>`: own a persistent llama-server for this run.
    // Held in `_local` so it lives until main_inner returns; dropping it kills
    // the process (Drop impl). Because the server is long-lived, its prefix KV
    // cache is reused across turns — the measured 33x prefill win.
    let mut _local: Option<rijo_core::local_server::LocalServer> = None;
    if let Some(model_path) = cli.local.as_deref() {
        let exe = match cli.local_server.as_deref() {
            Some(p) => std::path::PathBuf::from(p),
            None => rijo_core::paths::which_bin("llama-server")
                .ok_or("llama-server not found on PATH — pass --local-server <path>")?,
        };
        let mut server = rijo_core::local_server::start_for_model(
            &exe,
            std::path::Path::new(model_path),
            cli.local_port,
        )
        .map_err(|e| format!("cannot start llama-server: {e}"))?;
        if !server.wait_ready(std::time::Duration::from_secs(600)) {
            return Err(
                "llama-server did not become ready (check the model path and flags)".into(),
            );
        }
        eprintln!(
            "  local: {} @ {} (owned by Rijo Code, stopped on exit)",
            model_path,
            server.base_url()
        );
        config.llm.base_url = server.base_url();
        if cli.model.is_none() {
            // Keep the file name so the context-window table can resolve the
            // family (e.g. "gpt-oss" -> 131072). llama-server ignores the field.
            config.llm.model = std::path::Path::new(model_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("local")
                .to_string();
        }
        // llama-server ignores the key; a non-empty value skips onboarding.
        config.llm.api_key = "local".to_string();
        _local = Some(server);
    }

    if cli.list_providers {
        for tpl in rijo_core::providers::all() {
            print!("{}", rijo_core::providers::describe(&tpl));
            println!();
        }
        println!(
            "Active model: {}  @ {}",
            config.llm.model, config.llm.base_url
        );
        return Ok(());
    }

    // `rijo mcp list [server]` — inspect the configured MCP servers without a
    // model turn. Handled before the Executor is built (no brain, no indexing,
    // no provider, no trust prompt).
    if let Some(rijo_core::cli::Command::Mcp {
        action: rijo_core::cli::McpAction::List { server },
    }) = &cli.command
    {
        return mcp_list(&config, server.as_deref()).await;
    }

    // Two gates, two questions, two sets of flags.
    //
    // Onboarding asks for a provider and a key. No local one-shot makes a turn,
    // so none of them needs a provider — including `--index`.
    //
    // The trust question is about the workspace, so it stays for `--index`
    // (which reads the workspace) and is skipped for the rest (listing tools,
    // fetching the model, local measurements).
    //
    // Measured before this: install.ps1 hung at the onboarding prompt, and
    // `rijo --tools` stalled at the trust prompt on a fresh machine — both are
    // documented commands that must not be interactive.
    // `rijo mcp serve` is a headless server: no onboarding, no trust prompt,
    // no LLM and no background indexing.
    let mcp_serve = matches!(&cli.command, Some(rijo_core::cli::Command::Mcp { .. }));

    let local_measurement = cli.download_models
        || cli.tools
        || cli.bench
        || cli.dump_labels
        || cli.schema_report
        || cli.tui_snapshot
        || mcp_serve;
    let index_only = cli.index;

    let needs_provider = !(local_measurement || index_only);
    let needs_trust = !local_measurement;

    if needs_provider && rijo_core::onboarding::needed(&config) {
        match rijo_core::onboarding::run() {
            Some(cfg) => config = cfg,
            None => {
                eprintln!("Setup not finished. Start rijo again later or set RIJO_SKIP_ONBOARD=1.");
                std::process::exit(1);
            }
        }
    }

    // Trust prompt for new workspaces (skip via env or marker).
    if needs_trust
        && !rijo_core::trust::is_trusted(&config.workspace)
        && std::env::var("RIJO_SKIP_TRUST").as_deref() != Ok("1")
    {
        let trusted = rijo_core::trust::prompt_trust(&config.workspace);
        if !trusted {
            eprintln!("\nDirectory not trusted. Rijo Code exits.\nSet RIJO_SKIP_TRUST=1 to skip.");
            std::process::exit(0);
        }
        let _ = rijo_core::trust::mark_trusted(&config.workspace);
    }

    // Load .env file into sealed arena (keys never exposed to LLM)
    match rijo_core::trust::load_env_file(&config.workspace) {
        Ok(names) => {
            if !names.is_empty() {
                eprintln!("  .env loaded: {} variable(s) protected.", names.len());
            }
        }
        Err(e) => eprintln!("  reading .env failed: {e}"),
    }

    // One-shot runs (--query/--e2e/--bench/--tools/--schema-eval) don't
    // benefit from the background indexing of the workspace — it costs
    // a measured ~100 s.
    let oneshot = cli.query.is_some()
        || cli.e2e
        || cli.bench
        || cli.tools
        || cli.schema_eval
        || cli.schema_report
        || cli.dump_labels
        || cli.tui_snapshot
        || cli.index
        || cli.sync_export
        || cli.sync_import
        || cli.sync_status
        || cli.download_models
        || mcp_serve;
    let executor = rijo_core::executor::Executor::new_opts(config, !oneshot);
    let state = executor.state();

    if mcp_serve {
        if let Some(rijo_core::cli::Command::Mcp { action }) = &cli.command {
            match action {
                rijo_core::cli::McpAction::Serve => {
                    if let Err(e) = mcp::serve(state.clone()).await {
                        eprintln!("mcp serve failed: {e}");
                        rijo_core::tools::shutdown_cdp();
                        rijo_core::process::shutdown_all();
                        return Err(e);
                    }
                }
                // Handled earlier (before the Executor is built).
                rijo_core::cli::McpAction::List { .. } => {}
            }
        }
        rijo_core::tools::shutdown_cdp();
        rijo_core::process::shutdown_all();
        return Ok(());
    }

    if cli.download_models {
        // Install-time step: fetch the ONNX model + tokenizer into the one cache
        // directory. Before this existed the download happened on the first turn
        // per working directory (fastembed's default cache is ./fastembed_cache),
        // which put a 128 MB copy into every project folder.
        use rijo_core::brain::embed::{cache_size, ensure_model, EmbedConfig};
        let cfg = EmbedConfig {
            model_id: state.config.memory.embedding_model.clone(),
            dim: state.config.memory.embedding_dim,
            show_progress: true,
            ..EmbedConfig::default()
        };
        match ensure_model(&cfg) {
            Ok(dir) => {
                let mb = cache_size(&dir) as f64 / 1_048_576.0;
                println!("model cache: {} ({mb:.1} MB)", dir.display());
                println!("ready — later starts reuse this cache (no download).");
                rijo_core::tools::shutdown_cdp();
                rijo_core::process::shutdown_all();
                return Ok(());
            }
            Err(e) => {
                eprintln!("model download failed: {e}");
                rijo_core::process::shutdown_all();
                return Err(format!("model download failed: {e}"));
            }
        }
    }

    if cli.e2e {
        let spec: serde_json::Value = match std::fs::read_to_string("data/e2e_prompt.json")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
        {
            Some(s) => s,
            None => {
                eprintln!("e2e spec missing: data/e2e_prompt.json");
                return Ok(());
            }
        };
        let prompt = spec["prompt"].as_str().unwrap_or_default().to_string();
        let mut history: Vec<rijo_core::llm::ChatMessage> = Vec::new();
        match executor.run_turn(&prompt, &mut history).await {
            Ok(text) => println!("{text}"),
            Err(e) => {
                eprintln!("e2e run failed: {e}");
                rijo_core::tools::shutdown_cdp();
                rijo_core::process::shutdown_all();
                std::process::exit(1);
            }
        }
        rijo_core::tools::shutdown_cdp();
        rijo_core::process::shutdown_all();
        return Ok(());
    }

    if cli.sync_export || cli.sync_import || cli.sync_status {
        let dir = rijo_core::sync::sync_dir(&state.config);
        let targets = rijo_core::sync::export_targets(&state.config);
        let finish = |code: i32| {
            rijo_core::tools::shutdown_cdp();
            rijo_core::process::shutdown_all();
            let _ = code;
        };
        if cli.sync_status {
            println!("{}", rijo_core::sync::describe(&state.config));
            match rijo_core::sync::inspect(&dir) {
                Ok(stats) if !stats.is_empty() => {
                    println!("last export in {}:", dir.display());
                    for (k, v) in stats {
                        println!("  {k}: {v}");
                    }
                }
                _ => println!("no export found in {}", dir.display()),
            }
            finish(0);
            return Ok(());
        }
        if cli.sync_export {
            if targets.is_empty() {
                eprintln!(
                    "nothing to export: sync.workspaces is empty.\
                     \nThe brain index holds source code — add the repositories you want to share,\
                     \ne.g. sync:\n  workspaces:\n    - ~/Projects/rijo"
                );
                finish(1);
                return Err("sync.workspaces empty".into());
            }
            match rijo_core::sync::export(&state.config, &state.brain, &dir) {
                Ok(report) => {
                    println!("{}", report.summary());
                    println!("written to {}", dir.display());
                    finish(0);
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("sync export failed: {e}");
                    finish(1);
                    return Err(e);
                }
            }
        }
        if cli.sync_import {
            match rijo_core::sync::import(&state.config, &state.brain, &dir) {
                Ok(report) => {
                    println!("{}", report.summary());
                    finish(0);
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("sync import failed: {e}");
                    finish(1);
                    return Err(e);
                }
            }
        }
    }

    if cli.index {
        // One-shot runs (--query/--e2e) skip the background indexing on purpose.
        // This flag lets the brain be filled anyway — measured before it was
        // impossible from the CLI, so brain_search could never be verified in a
        // one-shot run.
        let embedder =
            rijo_core::brain::embed::Embedder::new(rijo_core::brain::embed::EmbedConfig::default());
        let started = std::time::Instant::now();
        match rijo_core::brain::indexer::index_workspace(
            &state.brain,
            &embedder,
            &state.config.workspace,
        ) {
            Ok(summary) => {
                println!("{summary}");
                println!("indexing took {} ms", started.elapsed().as_millis());
                rijo_core::tools::shutdown_cdp();
                rijo_core::process::shutdown_all();
                return Ok(());
            }
            Err(e) => {
                eprintln!("indexing failed: {e}");
                rijo_core::process::shutdown_all();
                return Err(e);
            }
        }
    }

    if cli.bench {
        let res = rijo_core::bench::run_bench(state).await;
        rijo_core::tools::shutdown_cdp();
        rijo_core::process::shutdown_all();
        return res;
    }

    if cli.tui_snapshot {
        // Check the look without a terminal: renders the views into a
        // test-backend buffer and prints them as text.
        for (title, frame) in rijo_tui::snapshot_frames(
            cli.width,
            cli.height,
            state.config.llm.effective_context_window().unwrap_or(0),
        ) {
            println!("── {title} ──");
            println!("{frame}");
        }
        return Ok(());
    }

    if cli.schema_report {
        println!("{}", executor.schema_report().await);
        return Ok(());
    }

    if cli.schema_eval {
        // Parse variants — unknown names are a hard error, not a
        // silent fallback to the base (otherwise you measure the wrong variant).
        let mut modes = Vec::new();
        for part in cli.variants.split(',') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            match rijo_core::schema_eval::Mode::parse(p) {
                Some(m) => modes.push(m),
                None => {
                    eprintln!(
                        "Unknown variant '{p}'. Allowed: full, no_emoji, first_sentence, no_param_desc, compact, minimal"
                    );
                    return Ok(());
                }
            }
        }
        if modes.is_empty() {
            modes = rijo_core::schema_eval::Mode::default_set();
        }
        let opts = rijo_core::schema_eval::EvalOptions {
            tasks_path: std::path::PathBuf::from("data/schema_tasks.json"),
            modes,
            repeats: cli.repeats,
            out_dir: state.config.data_dir.join("schema_eval"),
            limit: cli.limit,
            router: cli.router_eval,
        };
        let res = executor.run_schema_eval(&opts).await;
        rijo_core::tools::shutdown_cdp();
        rijo_core::process::shutdown_all();
        match res {
            Ok(table) => {
                println!("{table}");
                return Ok(());
            }
            Err(e) => {
                eprintln!("schema evaluation failed: {e}");
                std::process::exit(1);
            }
        }
    }

    if cli.tools {
        let names = state.tools.get_names();
        println!("Rijo Code — registered tools ({}):", names.len());
        for name in &names {
            if let Some(meta) = state.tools.get_meta(name) {
                println!("  {} {} — {}", meta.emoji, meta.name, meta.description);
            }
        }
        rijo_core::tools::shutdown_cdp();
        rijo_core::process::shutdown_all();
        return Ok(());
    }

    if cli.dump_labels {
        // Label tables for the router dataset: name + toolset in the exact
        // neuron order the model trains on. Written from the registry, so a
        // renamed/added tool cannot drift silently against the Python side.
        match rijo_core::router::dump_labels(&state.tools, &state.config.data_dir) {
            Ok(path) => {
                println!("{}", path.display());
                rijo_core::tools::shutdown_cdp();
                rijo_core::process::shutdown_all();
                return Ok(());
            }
            Err(e) => {
                eprintln!("dump-labels failed: {e}");
                rijo_core::process::shutdown_all();
                return Err(format!("dump-labels failed: {e}"));
            }
        }
    }

    if let Some(child_args) = &cli.child {
        if child_args.len() == 2 {
            rijo_core::tools::delegate_tool::run_child(
                std::path::Path::new(&child_args[0]),
                std::path::Path::new(&child_args[1]),
            )
            .await;
        }
        return Ok(());
    }

    if let Some(query) = &cli.query {
        let mut history: Vec<rijo_core::llm::ChatMessage> = Vec::new();
        match executor.run_turn(query, &mut history).await {
            Ok(text) => println!("{text}"),
            Err(e) => {
                eprintln!("query failed: {e}");
                rijo_core::tools::shutdown_cdp();
                rijo_core::process::shutdown_all();
                std::process::exit(1);
            }
        }
        // The learning review runs in the background; in a one-shot run it would
        // die with the process. Wait briefly so the lesson actually lands.
        if !executor
            .wait_for_learning(std::time::Duration::from_secs(60))
            .await
        {
            eprintln!("[learn] review still running — aborted on exit");
        }
        rijo_core::tools::shutdown_cdp();
        rijo_core::process::shutdown_all();
        return Ok(());
    }

    // Headless scripted session: one turn per stdin line, all in ONE process so
    // the provider's prompt cache applies across turns (and A/B measurements of
    // the router see the real cache behaviour, not a cold process each turn).
    if cli.repl {
        executor.run_single_turn().await;
        let _ = executor
            .wait_for_learning(std::time::Duration::from_secs(30))
            .await;
        rijo_core::tools::shutdown_cdp();
        rijo_core::process::shutdown_all();
        return Ok(());
    }

    // Interactive TUI
    let model_name = state.config.llm.model.clone();
    let tool_count = state.tools.get_names().len();
    let _pending_restart = rijo_core::selfupd::take_restart_marker();
    let (resume_session_id, resume_history) = if cli.resume {
        match rijo_core::tools::session_tool::load_recent_session(&state, 20) {
            Some((session_id, history)) => (Some(session_id), history),
            None => (None, Vec::new()),
        }
    } else {
        (None, Vec::new())
    };
    if let Some(session_id) = resume_session_id {
        if let Err(error) = executor.set_session_id(&session_id) {
            eprintln!("Could not resume session {session_id}: {error}");
        }
    }
    drop(state);
    let executor = std::sync::Arc::new(executor);
    let executor_for_wait = executor.clone();
    rijo_tui::run(executor, &model_name, tool_count, resume_history).await;
    // A learning review of the last turn runs in the background — wait briefly
    // for it instead of losing the lesson on exit.
    let _ = executor_for_wait
        .wait_for_learning(std::time::Duration::from_secs(30))
        .await;
    rijo_core::tools::shutdown_cdp();
    rijo_core::process::shutdown_all();

    if rijo_tui::REQUEST_RESTART.load(std::sync::atomic::Ordering::SeqCst) {
        println!("Rijo Code update done, restarting with --resume.");
        // Unix nutzt exec() (ersetzt den Prozess). Windows hat kein exec —
        // dort startet man einen Kindprozess und wartet nicht. Das Ergebnis
        // ist identisch: ein neuer Rijo Code-Prozess mit --resume.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new(std::env::current_exe().unwrap_or_default())
                .arg("--resume")
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .exec();
            eprintln!("restart failed: {err:?}");
        }
        #[cfg(not(unix))]
        {
            match std::process::Command::new(std::env::current_exe().unwrap_or_default())
                .arg("--resume")
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .spawn()
            {
                Ok(mut child) => {
                    let _ = child.wait();
                }
                Err(e) => eprintln!("restart failed: {e}"),
            }
        }
    }

    Ok(())
}

/// `rijo mcp list [server]` — the configured MCP servers, or one server's tools.
async fn mcp_list(config: &Config, server: Option<&str>) -> Result<(), String> {
    match server {
        None => {
            let servers = &config.mcp.servers;
            if servers.is_empty() {
                println!(
                    "No MCP servers configured. Add them under `mcp:` in {}.",
                    Config::default_path().display()
                );
                return Ok(());
            }
            print!("{}", render_server_list(servers));
            Ok(())
        }
        Some(name) => {
            let server = config
                .mcp
                .server(name)
                .ok_or_else(|| format!("unknown or disabled MCP server `{name}`"))?;
            let env: Vec<(String, String)> = server
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let client = rijo_core::mcp_client::McpClient::connect_stdio(
                &server.command,
                &server.args,
                &env,
            )
            .await?;
            let tools = client.list_tools().await?;
            println!("{} tool(s) on `{}`:", tools.len(), server.name);
            for t in &tools {
                println!(
                    "  {:<24} {}",
                    t.name,
                    t.description.as_deref().unwrap_or("")
                );
            }
            Ok(())
        }
    }
}

/// Human-readable rendering of the configured servers (unit-tested).
fn render_server_list(servers: &[rijo_core::config::McpServerConfig]) -> String {
    let mut out = format!("{} MCP server(s) configured:\n", servers.len());
    for s in servers {
        let state = if s.enabled { "enabled" } else { "disabled" };
        out.push_str(&format!(
            "  {:<16} [{}] {} {}\n",
            s.name,
            state,
            s.command,
            s.args.join(" ")
        ));
        let read_only = if s.read_only_tools.is_empty() {
            "none — every call needs approval".to_string()
        } else {
            s.read_only_tools.join(", ")
        };
        out.push_str(&format!("      read-only tools: {read_only}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rijo_core::config::McpServerConfig;

    fn server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: "srv".to_string(),
            args: vec!["--x".to_string()],
            env: Default::default(),
            read_only_tools: Vec::new(),
            enabled: true,
        }
    }

    #[test]
    fn the_server_list_shows_name_command_and_read_only_tools() {
        let mut s = server("self");
        s.read_only_tools = vec!["read_file".to_string(), "list".to_string()];
        let text = render_server_list(&[s]);
        assert!(text.contains("1 MCP server(s) configured"), "{text}");
        assert!(text.contains("self"), "{text}");
        assert!(text.contains("srv --x"), "{text}");
        assert!(text.contains("read_file, list"), "{text}");
    }

    #[test]
    fn an_empty_allowlist_says_every_call_needs_approval() {
        let text = render_server_list(&[server("x")]);
        assert!(text.contains("every call needs approval"), "{text}");
    }
}
