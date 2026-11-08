//! Terminal tool — execute shell commands with full process management
//!
//! Uses `std::process::Command` on blocking threads. Supports:
//! - Foreground execution with enforced timeout (poll + kill)
//! - Background process management
//! - Working directory control
//! - PTY mode for interactive CLIs (via tokio async process)
//! - Sudo commands: user confirmation via TUI, password from sealed env

use super::build_hint::build_hint;
use crate::tools::{tool_error, JsonSchema, Tool, ToolMeta, ToolResult};
use crate::AppState;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Upper bound for a single stream in a tool result. Both stdout AND stderr are
/// capped — a chatty stderr must not flood the context either.
const MAX_OUTPUT_CHARS: usize = 50_000;

/// Cap `s` at [`MAX_OUTPUT_CHARS`] characters, returning the text and whether it
/// was cut.
fn cap_output(s: String) -> (String, bool) {
    if s.chars().count() > MAX_OUTPUT_CHARS {
        let cut: String = s.chars().take(MAX_OUTPUT_CHARS).collect();
        (
            format!("{cut}...\n[Output truncated at {MAX_OUTPUT_CHARS} chars]"),
            true,
        )
    } else {
        (s, false)
    }
}

pub struct TerminalTool;

impl Tool for TerminalTool {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "terminal",
            description: "Execute a shell command (stdout, stderr, exit code).",
            emoji: "💻",
            toolset: "terminal",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");

        if command.is_empty() {
            return tool_error("command is required");
        }

        // ── Native-tool enforcement (CrowdStrike/ActPlane pattern) ──
        // Prompt guidance is advisory; this gate is deterministic.
        if let Some(native) = native_tool_redirect(command) {
            return tool_error(format!(
                "REJECTED — use the native tool instead (100-1000x faster): {native}. \
                 terminal is only for build/test/git/CLIs."
            ));
        }

        let trimmed = command.trim_start();
        let uses_sudo = trimmed
            .split_whitespace()
            .next()
            .map(|program| program.eq_ignore_ascii_case("sudo"))
            .unwrap_or(false);

        // ── Sudo approval gate ─────────────────────────────────────────────
        // Every sudo command needs explicit user confirmation (like delete_file).
        // The sudo password is stored in the sealed env and injected automatically.
        if uses_sudo {
            let approved = crate::guard::env_approved()
                || crate::guard::dialog_blocking(
                    &state,
                    format!(
                        "sudo — runs as root\n\n{}\n\nAllow this?",
                        command.chars().take(400).collect::<String>()
                    ),
                )
                .unwrap_or(false);
            if !approved {
                crate::guard::log_decision("sudo", command, "root privileges", "denied");
                return tool_error(
                    "BLOCKED (sudo): not approved. Commands with root privileges need the user's \
                     confirmation in the TUI — you cannot grant it yourself. Tell the user what \
                     you wanted to do and why, then stop."
                        .to_string(),
                );
            }
            crate::guard::log_decision("sudo", command, "root privileges", "approved");
        }

        // ── Dangerous-command gate (central: guard.rs) ────────────────────
        // Applies to ALL execution paths (this tool, execute_code, payloads
        // started via a script) and cannot be unlocked by the model itself:
        // approval comes from the TUI dialog or in advance from the user
        // (RIJO_APPROVE_DANGEROUS in the process environment).
        if let Some(why) = crate::guard::command_reason(command) {
            if let Err(msg) = crate::guard::approve_blocking(&state, "terminal", command, why) {
                return tool_error(msg);
            }
        } else if let Some((file, why)) =
            crate::guard::script_file_reason(command, &state.config.workspace)
        {
            // `bash cleanup.sh` does not carry the pattern itself — the
            // payload lives in the file, so the file is checked as well.
            let detail = format!("{command}   [{file}]");
            if let Err(msg) = crate::guard::approve_blocking(&state, "terminal", &detail, why) {
                return tool_error(msg);
            }
        }

        // Where may the process run? (ADR 0018) — decides the execution context,
        // not whether the user is asked (that stays with the guard above).
        let sandbox = crate::sandbox::Sandbox::detect(&state.config);

        let background = args
            .get("background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let timeout = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(state.config.terminal_timeout);
        // A command without an explicit workdir runs in the workspace, not in
        // the process cwd. The desktop host starts from wherever the binary was
        // launched and serves several projects, so inheriting cwd made a run in
        // one workspace operate on another checkout. A relative `workdir` is
        // resolved against the workspace for the same reason.
        let workdir_owned = resolve_workdir(
            args.get("workdir").and_then(|v| v.as_str()),
            &state.config.workspace,
        );
        let workdir_owned = workdir_owned.to_string_lossy().to_string();
        let workdir = Some(workdir_owned.as_str());

        if background {
            if uses_sudo {
                return tool_error("sudo commands must run in the foreground so the password can be delivered safely over stdin");
            }
            if !sandbox.is_off() {
                return tool_error(format!(
                    "sandbox: background execution is not sandboxed yet \
                     (sandbox.backend={}); run it in the foreground or set \
                     `sandbox.backend: off`.",
                    sandbox.label()
                ));
            }
            return self.exec_background(command, workdir, &state.config.data_dir);
        }

        // A command that sends itself to the background (`&`, `nohup`,
        // `setsid`) must NOT run in the foreground: bash finishes immediately,
        // but the server inherits the pipes — the reader then waits for EOF,
        // which only arrives when the server dies. That is exactly where the
        // turn hung on 15.09.2026 with `python3 -m http.server … & sleep 1; curl`.
        // So it goes into the process registry (output to files, returns
        // immediately, controllable via process_manage).
        if Self::self_backgrounds(command) {
            if uses_sudo {
                return tool_error("sudo commands cannot self-background; use a foreground call");
            }
            if !sandbox.is_off() {
                return tool_error(format!(
                    "sandbox: a self-backgrounding command is not sandboxed yet \
                     (sandbox.backend={}); set `sandbox.backend: off` or drop the \
                     `&`/`nohup`/`start`.",
                    sandbox.label()
                ));
            }
            let res = self.exec_background(command, workdir, &state.config.data_dir);
            if let Ok(mut v) = serde_json::from_str::<Value>(&res) {
                v["detected_backgrounding"] = json!(true);
                // Name the syntax that was actually detected: on a cmd.exe shell
                // it is `start`, not `&`.
                let syntax = if Self::shell_is_cmd() {
                    "'start'"
                } else {
                    "'&'/'nohup'/'setsid'"
                };
                v["message"] = json!(format!(
                    "{} The command contained {syntax} — it runs as a background \
                     process now (stdout/stderr go to files, nothing is lost). Check it with \
                     process_manage (poll/log/wait) and verify a server in a SEPARATE terminal \
                     call, e.g. curl -s -o /dev/null -w \"%{{http_code}}\" http://localhost:PORT/.",
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("Process started.")
                ));
                return v.to_string();
            }
            return res;
        }

        with_policy_note(
            self.exec_foreground(command, timeout, workdir, uses_sudo, &sandbox),
            command,
            &state.config.workspace,
        )
    }
}

impl TerminalTool {
    /// Does the command send itself to the background?
    ///
    /// Reads the shell that is actually in use (see `shell_command`): `&`,
    /// `nohup`, `setsid` are POSIX, `start` is the cmd.exe counterpart.
    fn self_backgrounds(command: &str) -> bool {
        Self::self_backgrounds_in(command, Self::shell_is_cmd())
    }

    /// True when the resolved shell is `cmd.exe` rather than bash.
    fn shell_is_cmd() -> bool {
        crate::tools::terminal_tool::shell_command()
            .0
            .eq_ignore_ascii_case("cmd.exe")
    }

    /// The detection with the shell choice made explicit, so both branches are
    /// testable on either platform without hiding bash.
    ///
    /// POSIX: `&` as its own token, `cmd&`, `nohup`, `setsid`. `&&`, `2>&1` and
    /// `&>file` are NOT background, and content inside quotes does not count.
    ///
    /// cmd.exe has no `&`-as-background at all; its counterpart is `start`
    /// (`start /b prog`, `start "" prog`). The rule only applies when that shell
    /// is in use, because `start` is also an ordinary word — `echo start` must
    /// not be read as a background launch.
    fn self_backgrounds_in(command: &str, cmd_shell: bool) -> bool {
        let unquoted = Self::without_quoted(command);

        if cmd_shell {
            // `start` at the beginning, or after a separator (`cd x && start b`).
            let tokens: Vec<String> = unquoted
                .to_ascii_lowercase()
                .split_whitespace()
                .map(String::from)
                .collect();
            return tokens.first().map(|t| t == "start").unwrap_or(false)
                || tokens.iter().enumerate().any(|(i, t)| {
                    t == "start" && i > 0 && matches!(tokens[i - 1].as_str(), "&" | "&&" | "|")
                });
        }

        if unquoted.contains("nohup ") || unquoted.contains("setsid ") {
            return true;
        }
        unquoted.split_whitespace().any(|t| {
            t == "&"
                || (t.ends_with('&')
                    && t != "&&"
                    && !t.starts_with("&>")
                    && !t.contains(">&")
                    && !t.starts_with('>'))
        })
    }

    /// The command with single- and double-quoted spans removed, so that a `&`
    /// inside a commit message or a grep pattern is not mistaken for one.
    fn without_quoted(command: &str) -> String {
        let mut unquoted = String::new();
        let mut quote: Option<char> = None;
        for c in command.chars() {
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None if c == '\'' || c == '"' => quote = Some(c),
                None => unquoted.push(c),
            }
        }
        unquoted
    }

    /// Prepare sudo without ever interpolating the sealed password into shell
    /// text or argv. The password is returned separately for stdin delivery.
    fn resolve_command(command: &str) -> (String, bool, Option<String>) {
        let trimmed = command.trim_start();
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        if end == trimmed.len() || !trimmed[..end].eq_ignore_ascii_case("sudo") {
            return (command.to_string(), false, None);
        }
        let rest = trimmed[end..].trim_start();
        let password = crate::trust::get_sealed("SUDO_PASSWORD");
        let shell_text = if password.is_some() {
            format!("sudo -S -- {rest}")
        } else {
            format!("sudo {rest}")
        };
        (shell_text, true, password)
    }

    fn exec_foreground(
        &self,
        command: &str,
        timeout: u64,
        workdir: Option<&str>,
        uses_sudo: bool,
        sandbox: &crate::sandbox::Sandbox,
    ) -> ToolResult {
        let (resolved, _uses_sudo, sudo_password) = if uses_sudo {
            Self::resolve_command(command)
        } else {
            (command.to_string(), false, None)
        };
        let actual_cmd = &resolved;

        use std::process::{Command, Stdio};

        let (shell, shell_flag) = crate::tools::terminal_tool::shell_command();
        let plan = match sandbox.plan(&shell, &[shell_flag.to_string(), actual_cmd.clone()]) {
            Ok(p) => p,
            Err(e) => return tool_error(format!("sandbox: {e}")),
        };

        let mut cmd = Command::new(&plan.program);
        cmd.args(&plan.args);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        if sudo_password.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            // An agent command must never wait for input. With an inherited
            // console handle, `grep` (or any filter) invoked without a file
            // blocks on stdin until the timeout — measured: one
            // `git ls-files … | xargs grep …` with an empty list burned 180 s.
            cmd.stdin(Stdio::null());
        }
        // Own process group: that way the timeout kills the WHOLE group and
        // not just the shell — otherwise a grandchild process (e.g. a server
        // started with `&`) survives every abort as an orphan.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }

        let start = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(timeout.min(600));

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return tool_error(format!("Failed to execute command: {e}")),
        };
        let pid = child.id();

        if let Some(password) = sudo_password {
            use std::io::Write;
            let Some(mut stdin) = child.stdin.take() else {
                crate::process::kill_tree(pid);
                let _ = child.kill();
                return tool_error("sudo password channel was not created");
            };
            if let Err(e) = stdin
                .write_all(password.as_bytes())
                .and_then(|()| stdin.write_all(b"\n"))
            {
                crate::process::kill_tree(pid);
                let _ = child.kill();
                return tool_error(format!("could not deliver sudo password over stdin: {e}"));
            }
            // Closing the pipe after the one password line prevents sudo (or
            // the command) from waiting indefinitely for more input.
        }

        // Collect output continuously in threads. Two reasons:
        // (1) A full pipe buffer (~64 KB) blocks the child — without
        //     concurrent reading, a chatty command runs into the
        //     timeout even though it would have finished.
        // (2) After the child exits, the turn must not wait for EOF while a
        //     grandchild holds the write end.
        let out_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let err_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let (eof_tx, eof_rx) = std::sync::mpsc::channel::<()>();
        match child.stdout.take() {
            Some(p) => spawn_reader(p, out_buf.clone(), eof_tx.clone()),
            None => {
                let _ = eof_tx.send(());
            }
        }
        match child.stderr.take() {
            Some(p) => spawn_reader(p, err_buf.clone(), eof_tx.clone()),
            None => {
                let _ = eof_tx.send(());
            }
        }
        drop(eof_tx);

        // No more polling: one thread blocks on wait() and reports the
        // end over a channel. Before, EVERY command cost at least
        // one poll-interval sleep (50 ms).
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = status_tx.send(child.wait());
        });

        let status =
            match status_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Ok(st)) => st,
                Ok(Err(e)) => return tool_error(format!("Command failed: {e}")),
                Err(_) => {
                    // Kill the whole tree, not just the shell: otherwise a
                    // grandchild (e.g. a server behind `&`) keeps running as an
                    // orphan. `kill_tree` is `killpg` on Unix and
                    // `taskkill /T` on Windows.
                    crate::process::kill_tree(pid);
                    let _ = status_rx.recv_timeout(Duration::from_secs(5));
                    // A timeout is exactly when partial output is most useful:
                    // give the readers a moment, then keep whatever arrived.
                    std::thread::sleep(Duration::from_millis(50));
                    let out_bytes = out_buf.lock().map(|g| g.clone()).unwrap_or_default();
                    let err_bytes = err_buf.lock().map(|g| g.clone()).unwrap_or_default();
                    let (partial_out, out_cut) =
                        cap_output(String::from_utf8_lossy(&out_bytes).to_string());
                    let (partial_err, err_cut) =
                        cap_output(String::from_utf8_lossy(&err_bytes).to_string());
                    let mm = start.elapsed().as_millis() as u64;
                    return json!({
                        "output": partial_out,
                        "exit_code": -1,
                        "error": format!(
                            "Timeout after {timeout}s (killed). A filter without a file \
                             argument (`grep`/`find` behind `xargs`) waits on stdin, which \
                             agent commands never receive — pass explicit arguments.\n{partial_err}"
                        ),
                        "duration_ms": mm,
                        "truncated": out_cut || err_cut,
                        "cwd": workdir.unwrap_or("."),
                        "sandbox": plan.backend,
                        "timed_out": true
                    })
                    .to_string();
                }
            };

        // Wait briefly for EOF, then take what is there: if a grandchild keeps
        // the pipes open, the output is still complete.
        let grace = Instant::now() + Duration::from_millis(300);
        let mut eofs = 0;
        while eofs < 2 {
            let left = grace.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match eof_rx.recv_timeout(left) {
                Ok(()) => eofs += 1,
                Err(_) => break,
            }
        }

        let out_bytes = out_buf.lock().map(|g| g.clone()).unwrap_or_default();
        let err_bytes = err_buf.lock().map(|g| g.clone()).unwrap_or_default();
        let elapsed = start.elapsed();
        let (stdout, out_cut) = cap_output(String::from_utf8_lossy(&out_bytes).to_string());
        let (stderr, err_cut) = cap_output(String::from_utf8_lossy(&err_bytes).to_string());
        let exit_code = status.code().unwrap_or(-1);
        let truncated = out_cut || err_cut;

        let mut result = json!({
            "output": stdout,
            "exit_code": exit_code,
            "error": stderr,
            "duration_ms": elapsed.as_millis() as u64,
            "truncated": truncated,
            "cwd": workdir.unwrap_or("."),
            "sandbox": plan.backend
        });
        // build-acceleration hint for slow cargo calls
        let ms = elapsed.as_millis() as u64;
        if let Some(hint) = build_hint(command, workdir, ms) {
            result["build_hint"] = json!(hint);
        }
        // Soft nudge (never a block): an interpreter that just did FILE work is a
        // detour — the native tools are direct. A legitimate script still runs;
        // the model just gets told, at the moment it happens.
        if interpreter_file_io(command) {
            result["hint"] = json!(
                "This ran an interpreter for FILE work. Next time use read_file, \
                 search_files, patch or write_file — direct, paginated and guarded."
            );
        }
        result.to_string()
    }

    /// Starts a background process in the registry (`crate::process`):
    /// stdout/stderr go to files (no unread pipes that block the process),
    /// the handle is kept → process_manage can poll, log, wait, write and kill.
    fn exec_background(
        &self,
        command: &str,
        workdir: Option<&str>,
        data_dir: &std::path::Path,
    ) -> ToolResult {
        match crate::process::spawn(command, workdir, data_dir) {
            Ok((id, pid)) => json!({
                "output": "",
                "exit_code": 0,
                "error": "",
                "pid": pid,
                "session_id": id,
                "background": true,
                "message": format!("Process started (id {id}, pid {pid}). Use process_manage poll/log/wait/kill to control it."),
            })
            .to_string(),
            Err(e) => tool_error(format!("Failed to start background process: {e}")),
        }
    }
}

// ── Native-tool enforcement ──────────────────────────────────────────────

/// Reads a pipe into a buffer in its own thread and reports EOF.
///
/// Why a thread and not `read_to_end` after the child exits: if a grandchild
/// process holds the write end open (a started server), EOF only comes with
/// its end — the turn hangs. And reading continuously additionally prevents
/// a full pipe buffer (~64 KB) from blocking the child.
fn spawn_reader<R: std::io::Read + Send + 'static>(
    mut pipe: R,
    buf: Arc<std::sync::Mutex<Vec<u8>>>,
    eof: std::sync::mpsc::Sender<()>,
) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut g) = buf.lock() {
                        if g.len() < 400_000 {
                            g.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let _ = eof.send(());
    });
}

/// Das Shell-Programm und sein Flag für `terminal` und `process_manage`.
///
/// Parität: bash auf Windows wenn verfügbar (Git Bash, MSYS2, WSL), sonst
/// cmd.exe. Der System-Prompt, die Guard-Muster und die Selbst-Hintergrundisierungs-
/// Erkennung (`&`, `nohup`, `setsid`) sind POSIX formuliert — mit bash bleiben
/// sie auf beiden Plattformen woertlich unverändert.
///
/// Der zurueckgegebene Pfad ist **ohne Anfuehrungszeichen**: er geht direkt an
/// `Command::new`, und dort waeren sie Teil des Programmnamens.
/// `"C:\Program Files\Git\bin\bash.exe"` als Programmname ergibt unter Windows
/// `os error 123` (ERROR_INVALID_NAME) — nicht "nicht gefunden", sondern
/// "ungueltiger Name". Wer den Pfad in einen Shell-String einsetzt, muss ihn
/// selbst quoten.
pub(crate) fn shell_command() -> (String, &'static str) {
    if !cfg!(target_os = "windows") {
        return ("bash".to_string(), "-c");
    }
    // Windows: bash bevorzugt. Git Bash liegt typischerweise hier (nicht im PATH):
    let bash_paths = [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
        r"C:\msys64\usr\bin\bash.exe",
    ];
    for path in &bash_paths {
        if std::path::Path::new(path).is_file() {
            return ((*path).to_string(), "-c");
        }
    }
    // Fallback: cmd.exe (bash nicht gefunden)
    ("cmd.exe".to_string(), "/C")
}

/// Resolve the command's working directory against the workspace.
///
/// `None` means "the workspace"; a relative value is joined to it. Only an
/// absolute path is taken as given. Without this, a shell command inherited the
/// directory the desktop binary was launched from.
fn resolve_workdir(raw: Option<&str>, workspace: &std::path::Path) -> std::path::PathBuf {
    match raw {
        Some(dir) => {
            let expanded = shellexpand::tilde(dir).to_string();
            let path = std::path::PathBuf::from(expanded);
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        }
        None => workspace.to_path_buf(),
    }
}

/// A read-only terminal pipeline that has a native equivalent gets a one-line
/// correction attached to its result, so the next step uses the cheaper tool.
fn with_policy_note(result: ToolResult, command: &str, workspace: &std::path::Path) -> ToolResult {
    let Some(note) = policy_note(command, workspace) else {
        return result;
    };
    match serde_json::from_str::<Value>(&result) {
        Ok(mut value) => {
            value["policy_note"] = json!(note);
            value.to_string()
        }
        Err(_) => result,
    }
}

fn policy_note(command: &str, workspace: &std::path::Path) -> Option<String> {
    if let Some(note) = inspection_pipeline_note(command) {
        return Some(note.to_string());
    }
    cd_outside_workspace_note(command, workspace)
}

/// Flags a `cd` to an absolute path outside the selected workspace.
///
/// Regression for the 2026-10-17 desktop run: the model carried an absolute
/// path from an earlier turn, `cd`-ed into the Rijo checkout and analysed that
/// repository while the workspace was an empty `test2`. The command still runs
/// (reading elsewhere is not forbidden), but the result names the selected
/// project so the next step can correct course.
fn cd_outside_workspace_note(command: &str, workspace: &std::path::Path) -> Option<String> {
    let normalized_workspace = workspace
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    for segment in command.split([';', '&', '|', '\n']) {
        let segment = segment.trim();
        let Some(rest) = segment.strip_prefix("cd ") else {
            continue;
        };
        let target = rest.trim().trim_matches(|c| c == '"' || c == '\'');
        if target.is_empty() || target == "-" || target == "~" {
            continue;
        }
        let normalized = target.replace('\\', "/").to_lowercase();
        let has_drive = normalized.chars().nth(1).is_some_and(|c| c == ':')
            && normalized
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic());
        let is_absolute = normalized.starts_with('/') || has_drive;
        if is_absolute && !normalized.starts_with(&normalized_workspace) {
            return Some(format!(
                "This command changed directory to `{target}`, which is outside the selected \
                 workspace ({}). The shell already starts in the workspace; use relative paths \
                 unless the user asked you to work elsewhere.",
                workspace.display()
            ));
        }
    }
    None
}

/// Guidance for the pipelines the native-tool gate deliberately lets through.
///
/// A pipeline into `xargs` runs its command with **no file argument** when the
/// input list is empty; `grep` then reads stdin and blocks until the terminal
/// timeout. Measured on 2026-10-17: `git ls-files "*tests*" | xargs grep -c …`
/// spent 180 s that way. The result carries this note so the model switches to
/// `search_files`, which is ripgrep-backed and needs no shell.
fn inspection_pipeline_note(command: &str) -> Option<&'static str> {
    if !command.contains('|') {
        return None;
    }
    let lower = command.to_ascii_lowercase();
    if lower.contains("xargs") {
        return Some(
            "This pipeline ran a command with no file argument (empty input list), which waits \
             on stdin until the timeout. Use `search_files` (target 'content') for searches \
             instead of `xargs`.",
        );
    }
    let lists_files = lower.contains("git ls-files") || lower.contains("find ");
    if lists_files && (lower.contains("grep") || lower.starts_with("rg ") || lower.contains(" rg "))
    {
        return Some(
            "Content search over listed files belongs to `search_files` (target 'content'); \
             it is ripgrep-backed and needs no shell pipeline.",
        );
    }
    None
}

/// The first command that actually does something.
///
/// Setup segments (`cd`, `export`, `set`) and `echo` labels are skipped, so
/// `cd site && grep …` is judged as the grep it is. Pipelines are judged by
/// their first real stage: `cargo test | grep error` stays in the shell.
fn first_real_stage(command: &str) -> Option<&str> {
    for segment in command.split(['|', ';', '&', '\n']) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let first = segment.split_whitespace().next().unwrap_or("");
        let first = first
            .trim_start_matches("./")
            .rsplit('/')
            .next()
            .unwrap_or(first);
        if matches!(first, "cd" | "pushd" | "popd" | "export" | "set" | "echo") {
            continue;
        }
        return Some(segment);
    }
    None
}

/// Maps shell commands to their native counterpart. Returns Some(hint) if
/// the command has to be intercepted.
fn native_tool_redirect(command: &str) -> Option<String> {
    // Judge the first *real* command: a leading `cd`, `export`, `set` or an
    // `echo` label is setup, not the action. Output redirection no longer
    // exempts a command — `grep … > out.txt` still reads the filesystem.
    let trimmed = first_real_stage(command)?;

    // First token of the command (after path expansion)
    let first = trimmed.split_whitespace().next()?.trim_start_matches("./");
    let first = first.rsplit('/').next()?;
    let args: Vec<String> = trimmed
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .collect();
    let has_flag = |short: char, long: &str| {
        let combined = format!("{long}=");
        let short_flag = format!("-{short}");
        args.iter().any(|t| {
            let t = t.as_str();
            t == long
                || t.starts_with(&combined)
                || t == short_flag
                || (t.starts_with('-') && !t.starts_with("--") && t.contains(short))
        })
    };

    let redirect = match first {
        "ls" | "dir" => "search_files (pattern '*', target 'files')",
        "cat" | "head" => "read_file (line numbers + pagination included)",
        // `tail -f` / `-F` / `--follow` follows a live log — that is monitoring,
        // not a read; it must run.
        "tail" if !has_flag('f', "--follow") => "read_file (line numbers + pagination included)",
        "tail" => return None,
        // `find` with an action (-exec/-delete/-ok) is doing work, not listing.
        "find"
            if !args.iter().any(|t| {
                matches!(
                    t.as_str(),
                    "-exec" | "-execdir" | "-delete" | "-ok" | "-print0"
                )
            }) =>
        {
            "search_files (target 'files')"
        }
        "find" => return None,
        "grep" | "rg" => "search_files (target 'content', ripgrep-backed)",
        // Read-only git inspection has its own tool. Writes stay in the shell.
        "git"
            if matches!(
                args.get(1).map(String::as_str),
                Some("status" | "log" | "diff" | "show" | "branch" | "ls-files")
            ) =>
        {
            "the `git` tool (status / log / diff / show / branch / files)"
        }
        // `sed` only as an in-place editor (`sed -i`); a `sed` that prints is a
        // filter and legitimately runs. `awk` is a transformer, never a search.
        "sed" if has_flag('i', "--in-place") => {
            "patch (targeted find-and-replace, reviewable diff)"
        }
        _ => return None,
    };
    Some(redirect.to_string())
}

/// Does an interpreter invocation really open/read/write files? Used to steer
/// `python`/`node` file work back to the native tools without banning code
/// execution for computation.
fn interpreter_file_io(command: &str) -> bool {
    const MARKERS: &[&str] = &[
        "open(",
        ".read(",
        ".readlines(",
        ".write(",
        ".writelines(",
        "read_text",
        "write_text",
        "pathlib",
        "os.walk",
        "glob.glob",
        "shutil.",
        "json.load",
        "json.dump",
        "csv.reader",
        "csv.writer",
        "fs.readfile",
        "readFileSync",
        "writeFileSync",
        "readFile(",
        "writeFile(",
        "appendFileSync",
        "createReadStream",
        "createWriteStream",
    ];
    MARKERS.iter().any(|m| command.contains(m))
}

lazy_static::lazy_static! {
    static ref SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The shell command to execute"
            },
            "background": {
                "type": "boolean",
                "description": "Background (returns session id)"
            },
            "timeout": {
                "type": "integer",
                "description": "Timeout s (default 180, max 600)",
                "minimum": 1,
                "maximum": 600
            },
            "workdir": {
                "type": "string",
                "description": "Working directory"
            }
        },
        "required": ["command"]
    }));
}

#[cfg(test)]
mod redirect_tests {
    use super::interpreter_file_io;
    use super::native_tool_redirect;

    #[test]
    fn sudo_password_never_enters_shell_text() {
        use super::TerminalTool;

        let secret = "pa'ss'; echo injected; #";
        crate::trust::set_sealed("SUDO_PASSWORD".into(), secret.into());
        let (command, uses_sudo, stdin_password) = TerminalTool::resolve_command("sudo id");
        crate::trust::set_sealed("SUDO_PASSWORD".into(), String::new());

        assert!(uses_sudo);
        assert!(
            command.contains("sudo -S"),
            "password should travel on stdin: {command}"
        );
        assert!(
            !command.contains(secret),
            "secret leaked into shell command: {command}"
        );
        assert!(
            !command.contains("injected"),
            "password was parsed as shell syntax: {command}"
        );
        assert_eq!(stdin_password.as_deref(), Some(secret));
    }

    #[test]
    fn simple_calls_are_redirected_to_native_tools() {
        for cmd in [
            "ls -la",
            "cat README.md",
            "head -20 Cargo.toml",
            "find . -name '*.rs'",
            "grep -rn foo src",
        ] {
            assert!(
                native_tool_redirect(cmd).is_some(),
                "{cmd} should be redirected"
            );
        }
    }

    #[test]
    fn pipelines_that_filter_command_output_are_allowed() {
        for cmd in [
            "cargo test 2>&1 | grep error",
            "npm run build | tail -20",
            "seq 1 10 > /tmp/x.txt",
            "echo start | wc -l",
        ] {
            assert!(
                native_tool_redirect(cmd).is_none(),
                "{cmd} must NOT be blocked"
            );
        }
    }

    /// Regression for the 2026-10-17 run: inspection calls hid behind pipelines
    /// and compound commands and bypassed the gate entirely.
    #[test]
    fn inspection_pipelines_are_judged_by_their_first_stage() {
        for cmd in [
            "ls -la; ls crates",
            "find . -name '*.rs' | wc -l",
            "git log --oneline | head -20",
            "git status --short; git log",
            "grep -rEo 'href=\"[^\"]+\"' *.html | sed 's/x/y/' | sort",
            "rg foo src | head",
            "grep -o \"swatch: '[a-z]*'\" assets/js/produkte.js > /tmp/out.txt",
            "cd test2 && git log",
            "echo '== refs ==' ; git status",
        ] {
            assert!(
                native_tool_redirect(cmd).is_some(),
                "{cmd} should be redirected"
            );
        }
    }

    /// An interpreter doing FILE work is flagged (a soft hint in the tool
    /// result), but it is NOT blocked — legitimate scripts must still run.
    #[test]
    fn interpreter_file_io_is_detected_but_not_blocked() {
        for cmd in [
            "python3 -c \"print(open('x.html').read())\"",
            "python -c \"Path('a').write_text('b')\"",
            "node -e \"require('fs').writeFileSync('x','y')\"",
        ] {
            assert!(interpreter_file_io(cmd), "{cmd} is file work");
            assert!(
                native_tool_redirect(cmd).is_none(),
                "{cmd} must still RUN (hint only, not a hard block)"
            );
        }
        for cmd in [
            "python3 -c \"print(sum(range(10)))\"",
            "node --check app.js",
            "python3 -m pytest",
        ] {
            assert!(!interpreter_file_io(cmd), "{cmd} is not file work");
        }
    }

    #[test]
    fn unrelated_commands_pass_through() {
        for cmd in [
            "cargo check -p rijo-core",
            "wc -l Cargo.lock",
            "node --check app.js",
            "git commit -m 'status'",
            "git push",
        ] {
            assert!(
                native_tool_redirect(cmd).is_none(),
                "{cmd} must NOT be blocked"
            );
        }
    }

    /// The redirect must not fire on commands that only *look* like a search:
    /// a live-log `tail -f`, a `find` that deletes/execs, and a `sed`/`awk` that
    /// transforms rather than searches.
    #[test]
    fn redirect_avoids_false_positives() {
        for cmd in [
            "tail -f server.log",
            "tail -F server.log",
            "tail --follow=name server.log",
            "find . -name '*.tmp' -delete",
            "find . -name '*.rs' -exec wc -l {} +",
            "sed -n '1,20p' README.md",
            "awk '{print $1}' data.txt",
            "awk -F, '{print $2}' data.csv",
        ] {
            assert!(
                native_tool_redirect(cmd).is_none(),
                "{cmd} must be allowed to run"
            );
        }
    }

    /// The same gate must still redirect the plain cases it was built for,
    /// including `sed -i` (an in-place edit → `patch`).
    #[test]
    fn redirect_still_catches_the_plain_cases() {
        assert!(native_tool_redirect("tail -20 server.log").is_some());
        assert!(native_tool_redirect("find . -name '*.rs'").is_some());
        assert!(native_tool_redirect("sed -i 's/a/b/' f.txt").is_some());
        assert!(native_tool_redirect("sed -i.bak 's/a/b/' f.txt").is_some());
    }

    #[test]
    fn self_backgrounding_commands_are_detected() {
        use super::TerminalTool;
        // The POSIX branch, with the shell made explicit so the assertion does
        // not change with the machine that runs it.
        const POSIX: bool = false;
        // The real incident: start a server and check it on the same line.
        assert!(TerminalTool::self_backgrounds_in(
            "cd /tmp/x && python3 -m http.server 8712 >/dev/null 2>&1 & sleep 1; curl -s http://localhost:8712/",
            POSIX
        ));
        assert!(TerminalTool::self_backgrounds_in(
            "python3 -m http.server 8000 &",
            POSIX
        ));
        assert!(TerminalTool::self_backgrounds_in("sleep 30&", POSIX));
        assert!(TerminalTool::self_backgrounds_in("nohup ./serve.sh", POSIX));
        assert!(TerminalTool::self_backgrounds_in(
            "setsid ./serve.sh",
            POSIX
        ));
    }

    /// The cmd.exe branch: `start` is the counterpart of `&`/`nohup`. Without
    /// this a server started with `start` would run in the foreground and the
    /// turn would hang on the inherited pipes — the incident of 15.09.2026, in
    /// the shell Windows falls back to when no bash is installed.
    #[test]
    fn cmd_shell_backgrounding_is_detected() {
        use super::TerminalTool;
        const CMD: bool = true;
        assert!(TerminalTool::self_backgrounds_in(
            "start /b server.exe",
            CMD
        ));
        assert!(TerminalTool::self_backgrounds_in(
            "cd app && start \"\" node server.js",
            CMD
        ));
        assert!(TerminalTool::self_backgrounds_in("START server.exe", CMD));
        // `start` as an ordinary word is not a launch.
        assert!(!TerminalTool::self_backgrounds_in("echo start", CMD));
        assert!(!TerminalTool::self_backgrounds_in("python server.py", CMD));
        // The POSIX forms are not cmd forms.
        assert!(!TerminalTool::self_backgrounds_in("sleep 30 &", CMD));
    }

    /// `self_backgrounds` must read the shell that is actually in use; this
    /// pins the delegation so a future edit cannot quietly drop it.
    #[test]
    fn self_backgrounds_follows_the_resolved_shell() {
        use super::TerminalTool;
        let cmd = "cd app && start \"\" node server.js";
        assert_eq!(
            TerminalTool::self_backgrounds(cmd),
            TerminalTool::self_backgrounds_in(cmd, TerminalTool::shell_is_cmd())
        );
    }

    #[test]
    fn normal_commands_are_not_treated_as_background() {
        use super::TerminalTool;
        // Both branches, explicitly: a false positive would send an ordinary
        // command into the process registry instead of running it in the turn.
        for cmd_shell in [false, true] {
            for cmd in [
                "cargo test",
                "a && b",
                "echo hello 2>&1 | tail -1",
                "make > build.log 2>&1",
                "echo 'a & b'",
                "grep -c '&' README.md",
                "git commit -m 'fix & forget'",
                "echo start",
                "restart the server",
            ] {
                assert!(
                    !TerminalTool::self_backgrounds_in(cmd, cmd_shell),
                    "{cmd} must NOT count as background (cmd shell: {cmd_shell})"
                );
            }
        }
    }
}
#[cfg(test)]
mod foreground_tests {
    use super::{inspection_pipeline_note, resolve_workdir, TerminalTool};

    /// Regression for the 2026-10-17 run: the desktop app ran shell commands in
    /// the directory it was launched from, so a session in `test2` operated on
    /// the Rijo checkout.
    #[test]
    fn a_command_runs_in_the_workspace_unless_a_path_is_given() {
        let workspace = std::path::Path::new("C:/work/test2");
        assert_eq!(resolve_workdir(None, workspace), workspace.to_path_buf());
        assert_eq!(
            resolve_workdir(Some("crates/core"), workspace),
            workspace.join("crates/core")
        );
        let absolute = if cfg!(windows) {
            "C:/other/repo"
        } else {
            "/other/repo"
        };
        assert_eq!(
            resolve_workdir(Some(absolute), workspace),
            std::path::PathBuf::from(absolute)
        );
    }
    use serde_json::Value;

    #[test]
    fn inspection_pipelines_get_a_note_but_normal_commands_do_not() {
        assert!(inspection_pipeline_note("git ls-files \"*tests*\" | xargs grep -c x").is_some());
        assert!(inspection_pipeline_note("git ls-files | grep foo").is_some());
        assert!(inspection_pipeline_note("cargo test").is_none());
        assert!(inspection_pipeline_note("git status --short").is_none());
        assert!(inspection_pipeline_note("echo x | wc -l").is_none());
    }

    /// Regression for the 2026-10-17 run: `grep` without a file waited on
    /// stdin for the full 180 s timeout. With stdin closed it must return at
    /// once, and a later command in the same line must still run.
    #[test]
    fn a_command_that_reads_stdin_returns_instead_of_hanging() {
        if TerminalTool::shell_is_cmd() {
            return; // `read` is a bash builtin; cmd.exe would fail for another reason
        }
        let mut config = crate::config::Config::default();
        config.sandbox.backend = "off".to_string();
        let sandbox = crate::sandbox::Sandbox::detect(&config);

        let started = std::time::Instant::now();
        let raw = TerminalTool.exec_foreground(
            "echo start; read line; echo done",
            10,
            None,
            false,
            &sandbox,
        );
        let value: Value = serde_json::from_str(&raw).expect("terminal result is JSON");

        assert!(!value["timed_out"].as_bool().unwrap_or(false), "{raw}");
        assert!(
            started.elapsed().as_secs() < 5,
            "stdin read blocked: {:?}",
            started.elapsed()
        );
        assert!(
            value["output"].as_str().unwrap_or("").contains("done"),
            "{raw}"
        );
    }
}

#[cfg(test)]
mod policy_note_tests {
    use super::with_policy_note;
    use serde_json::Value;
    use std::path::Path;

    #[test]
    fn the_policy_note_is_attached_to_the_result_json() {
        let workspace = Path::new("C:/work/test2");
        let raw = with_policy_note(
            r#"{"output":"","exit_code":0}"#.to_string(),
            "git ls-files | xargs grep -c x",
            workspace,
        );
        let value: Value = serde_json::from_str(&raw).expect("JSON");
        assert!(
            value["policy_note"]
                .as_str()
                .unwrap_or("")
                .contains("xargs"),
            "{raw}"
        );

        let plain = with_policy_note(r#"{"output":""}"#.to_string(), "cargo test", workspace);
        let value: Value = serde_json::from_str(&plain).expect("JSON");
        assert!(value.get("policy_note").is_none(), "{plain}");
    }

    /// Regression for the 2026-10-17 run: the model `cd`-ed into the Rijo
    /// checkout while the selected workspace was an empty `test2`.
    #[test]
    fn leaving_the_workspace_is_named_in_the_result() {
        let workspace = Path::new("C:/work/test2");
        let note = super::cd_outside_workspace_note("cd \"C:/work/rijo\"; cargo test", workspace)
            .expect("absolute cd outside the workspace must be flagged");
        assert!(note.contains("outside the selected workspace"), "{note}");

        assert!(
            super::cd_outside_workspace_note("cd crates/core && cargo test", workspace).is_none(),
            "a relative cd stays inside the workspace"
        );
        assert!(
            super::cd_outside_workspace_note("cd C:/work/test2/sub && cargo test", workspace)
                .is_none(),
            "an absolute cd inside the workspace is fine"
        );
        assert!(
            super::cd_outside_workspace_note("cargo test", workspace).is_none(),
            "commands without cd carry no note"
        );
    }
}
