use crate::tools::{tool_error, AsyncTool, JsonSchema, ToolMeta, ToolResult};
use crate::AppState;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const MAX_OUTPUT_BYTES: usize = 50_000;
const MAX_TIMEOUT_SECS: u64 = 600;

pub struct ExecuteCode;

#[async_trait]
impl AsyncTool for ExecuteCode {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "execute_code",
            description: "Run Python code that calls Hermes tools programmatically.",
            emoji: "⚡",
            toolset: "core",
            is_async: true,
        }
    }
    fn schema(&self) -> &JsonSchema {
        &EXECUTE_SCHEMA
    }

    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let code = args.get("code").and_then(|v| v.as_str()).unwrap_or("");
        if code.is_empty() {
            return tool_error("code is required");
        }
        let timeout_secs = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(state.config.terminal_timeout)
            .clamp(1, MAX_TIMEOUT_SECS);
        // The same gate as `terminal` — otherwise python3 is the way around a
        // blocked `rm -rf` (real run on 2026-09-14: blocked three
        // times, then deleted via `shutil.rmtree`).
        if let Some(why) = crate::guard::code_reason(code) {
            if let Err(msg) = crate::guard::approve_async(&state, "execute_code", code, why).await {
                return tool_error(msg);
            }
        }
        // Execute by spawning the resolved Python interpreter, wrapped by the
        // sandbox backend (ADR 0018). A `box` backend fails closed here.
        let sandbox = crate::sandbox::Sandbox::detect(&state.config);
        let plan = match sandbox.plan(python_interpreter(), &["-c".to_string(), code.to_string()]) {
            Ok(p) => p,
            Err(e) => return tool_error(format!("sandbox: {e}")),
        };
        let mut command = tokio::process::Command::new(&plan.program);
        command
            .args(&plan.args)
            // Code runs inside the workspace, not the launcher's directory.
            .current_dir(&state.config.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => return tool_error(format!("Execution failed: {e}")),
        };
        let pid = child.id();
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill().await;
            return tool_error("Execution failed: stdout pipe was not created");
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill().await;
            return tool_error("Execution failed: stderr pipe was not created");
        };

        // Drain continuously so the child cannot deadlock on a full pipe, but
        // retain only a bounded prefix. `Command::output()` buffers without a
        // limit and let an infinite printer exhaust the Rijo process.
        let stdout_task = tokio::spawn(read_capped(stdout));
        let stderr_task = tokio::spawn(read_capped(stderr));
        let status = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await;
        let timed_out = status.is_err();
        let exit_code = match status {
            Ok(Ok(status)) => status.code().unwrap_or(-1),
            Ok(Err(e)) => {
                if let Some(pid) = pid {
                    crate::process::kill_tree(pid);
                }
                let _ = child.kill().await;
                let _ = join_capped(stdout_task).await;
                let _ = join_capped(stderr_task).await;
                return tool_error(format!("Execution failed while waiting for Python: {e}"));
            }
            Err(_) => {
                if let Some(pid) = pid {
                    crate::process::kill_tree(pid);
                }
                let _ = child.kill().await;
                let _ = child.wait().await;
                -1
            }
        };
        let (output, out_truncated) = join_capped(stdout_task).await;
        let (stderr, err_truncated) = join_capped(stderr_task).await;
        let truncated = out_truncated || err_truncated;

        if timed_out {
            json!({
                "output": output,
                "stderr": stderr,
                "exit_code": exit_code,
                "error": format!("Execution timed out after {timeout_secs}s; process tree killed"),
                "timed_out": true,
                "truncated": truncated,
                "sandbox": plan.backend
            })
            .to_string()
        } else {
            json!({
                "output": output,
                "stderr": stderr,
                "exit_code": exit_code,
                "truncated": truncated,
                "sandbox": plan.backend
            })
            .to_string()
        }
    }
}

async fn read_capped<R>(mut reader: R) -> (String, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut retained = Vec::with_capacity(MAX_OUTPUT_BYTES);
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let keep = n.min(MAX_OUTPUT_BYTES.saturating_sub(retained.len()));
        retained.extend_from_slice(&chunk[..keep]);
        truncated |= keep < n;
    }
    let text = String::from_utf8_lossy(&retained).to_string();
    let mut chars = text.chars();
    let capped: String = chars.by_ref().take(MAX_OUTPUT_BYTES).collect();
    let char_truncated = chars.next().is_some();
    let truncated = truncated || char_truncated;
    if truncated {
        (
            format!("{capped}\n[Output truncated at {MAX_OUTPUT_BYTES} bytes]"),
            true,
        )
    } else {
        (capped, false)
    }
}

async fn join_capped(task: tokio::task::JoinHandle<(String, bool)>) -> (String, bool) {
    match tokio::time::timeout(Duration::from_secs(2), task).await {
        Ok(Ok(result)) => result,
        _ => (String::new(), true),
    }
}

/// The Python interpreter `execute_code` runs with.
///
/// `python3` is the name on Linux; Windows (and some macOS installs) ship only
/// `python` or the `py` launcher, so a hardcoded `python3` made the tool dead
/// there — the same class of bug as the `which` search for Chromium. The probe
/// runs `--version`, never the caller's code, and is cached for the process.
fn python_interpreter() -> &'static str {
    use std::sync::OnceLock;
    static FOUND: OnceLock<&'static str> = OnceLock::new();
    FOUND.get_or_init(|| {
        for candidate in ["python3", "python", "py"] {
            let found = std::process::Command::new(candidate)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if found {
                return candidate;
            }
        }
        // Nothing found: keep the Linux name so the error names what is missing.
        "python3"
    })
}

lazy_static::lazy_static! {
    static ref EXECUTE_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "code": { "type": "string", "description": "Python code to execute" },
            "timeout": { "type": "integer", "description": "Execution timeout in seconds (default from terminal.timeout, maximum 600)", "minimum": 1, "maximum": 600 }
        },
        "required": ["code"]
    }));
}
