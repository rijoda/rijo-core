//! Unified gate for destructive actions — across ALL execution paths.
//!
//! Why central: the gate only sat in `terminal` and moreover only checked the
//! shell text. `execute_code` starts `python3 -c <code>` — there a
//! `shutil.rmtree` ran through unchecked, although `rm -rf` as a command was
//! blocked. Real run (14.09.2026): the model tried `rm -rf` three times,
//! was blocked three times and then deleted the workspace via
//! `execute_code` in a single step.
//!
//! The limit, honestly: this is a heuristic over text, NOT a sandbox. Anyone
//! who switches the interpreter (node, perl), drops a script via `write_file`
//! and executes it, or base64-encodes the payload gets past it. The hurdle
//! rises, the attempt becomes visible — guarantees only come with a sandbox
//! (bwrap/Landlock) or a restricted user. See README.

use crate::ui_events::UiEvent;
use crate::AppState;
use std::sync::Arc;

/// Shell patterns that indicate a destructive effect.
const COMMAND_PATTERNS: &[(&str, &str)] = &[
    ("rm -rf", "recursive delete"),
    ("rm -fr", "recursive delete"),
    ("rm -r ", "recursive delete"),
    ("shred ", "secure overwrite"),
    ("truncate -s 0", "file truncate"),
    ("dd if=", "disk dump/write"),
    ("> /dev/sd", "raw disk write"),
    ("> /dev/nvme", "raw disk write"),
    ("mkfs", "filesystem format"),
    (":(){:|:&};:", "fork bomb"),
    ("chmod -R 777 /", "chmod 777 on root"),
    ("chown -R", "recursive ownership change"),
    ("crontab -r", "remove all cron jobs"),
    ("userdel", "delete user"),
    ("iptables -f", "flush firewall rules"),
    ("pkill -9", "mass kill"),
    ("kill -9 -1", "kill all processes"),
    ("curl | sh", "remote script exec"),
    ("curl|sh", "remote script exec"),
    ("curl | bash", "remote script exec"),
    ("curl|bash", "remote script exec"),
    ("wget | sh", "remote script exec"),
    ("wget | bash", "remote script exec"),
    ("git push --force", "force push"),
    ("git push -f", "force push"),
    ("git reset --hard", "history rewrite"),
    ("git clean -fd", "untracked files delete"),
    ("drop table", "sql drop"),
    ("drop database", "sql drop"),
    ("shutdown", "system shutdown"),
    ("reboot", "system reboot"),
    ("poweroff", "system poweroff"),
    // Windows — the app targets Windows first and these were missing entirely,
    // so "delete that folder recursively" was free while "rm -rf" was gated.
    ("del /f", "force delete"),
    ("del /s", "recursive delete"),
    ("del /q", "quiet delete"),
    ("rd /s", "recursive delete"),
    ("rmdir /s", "recursive delete"),
    ("remove-item -recurse", "recursive delete"),
    ("remove-item -force", "force delete"),
    ("format c:", "filesystem format"),
    ("format d:", "filesystem format"),
    ("format /q", "filesystem format"),
    ("diskpart", "disk partitioning"),
    ("reg delete", "registry delete"),
    ("cipher /w", "free-space wipe"),
];

/// Destructive APIs in script/interpreter payloads (Python and the usual
/// neighbours). Pure read/write operations are deliberately NOT included —
/// otherwise `execute_code` would be unusable for normal work.
const CODE_PATTERNS: &[(&str, &str)] = &[
    ("shutil.rmtree", "recursive delete"),
    ("rmtree(", "recursive delete"),
    ("os.removedirs", "recursive delete"),
    ("os.rmdir", "remove directory"),
    ("os.remove", "delete file"),
    ("os.unlink", "delete file"),
    (".unlink(", "delete file"),
    (".rmdir(", "remove directory"),
    ("os.truncate", "file truncate"),
    ("fileutils.rm_rf", "recursive delete"),
    ("fs.rmsync", "recursive delete"),
    ("fs.rm(", "recursive delete"),
    ("fs.unlink", "delete file"),
    ("rimraf", "recursive delete"),
];

/// Does `needle` occur where a command can start?
///
/// Only at the start, after whitespace, after a shell separator, or inside
/// quotes. Plain `contains` would let `model /f` match `del /f` and `word /s`
/// match `rd /s` — the short Windows patterns make that difference real.
fn matches_at_boundary(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(offset) = haystack[from..].find(needle) {
        let at = from + offset;
        let boundary = at == 0
            || haystack[..at].chars().next_back().is_some_and(|c| {
                c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '`' | '\'' | '"')
            });
        if boundary {
            return true;
        }
        from = at + needle.len();
    }
    false
}

/// Reason why a shell command counts as destructive.
pub fn command_reason(command: &str) -> Option<&'static str> {
    let lower = command.to_lowercase();
    for (pat, why) in COMMAND_PATTERNS {
        if matches_at_boundary(&lower, pat) {
            return Some(why);
        }
    }
    // `find ... -delete` needs two parts, hence separate.
    if lower.contains("find ") && lower.contains("-delete") {
        return Some("recursive delete");
    }
    None
}

/// Reason why a code payload (Python/Node/…) counts as destructive.
/// Also includes the shell patterns, because `os.system("rm -rf …")` would
/// otherwise stay undetected.
pub fn code_reason(code: &str) -> Option<&'static str> {
    if let Some(why) = command_reason(code) {
        return Some(why);
    }
    let lower = code.to_lowercase();
    for (pat, why) in CODE_PATTERNS {
        if lower.contains(pat) {
            return Some(why);
        }
    }
    None
}

/// Is the call trying to approve itself? That is always a failed attempt —
/// approval comes exclusively from the user.
pub fn self_approval_attempt(text: &str) -> bool {
    text.to_uppercase().contains("RIJO_APPROVE_DANGEROUS")
}

/// Rule 1: a payload that unlocks itself is rejected outright.
fn self_approval_error(what: &str, why: &str) -> String {
    format!(
        "BLOCKED ({why}): this {what} call tries to approve itself \
         (RIJO_APPROVE_DANGEROUS). Approval can only come from the user. \
         Ask the user to confirm; do NOT retry the same action in another tool."
    )
}

/// Audit trail: every gate decision as its own line.
///
/// `tool_trace.jsonl` only shows THAT a call failed. Here the decision itself
/// is recorded: denied headless, confirmed by the user, or a rejected
/// self-approval. Path: `~/.rijo/data/guard_trace.jsonl`
/// (redirectable for tests via `RIJO_AUDIT_LOG`).
pub fn log_decision(what: &str, detail: &str, why: &str, decision: &str) {
    let path = match std::env::var("RIJO_AUDIT_LOG") {
        Ok(p) if !p.trim().is_empty() => std::path::PathBuf::from(p),
        _ => {
            let dir = crate::paths::rijo_home().join("data");
            let _ = std::fs::create_dir_all(&dir);
            dir.join("guard_trace.jsonl")
        }
    };
    let record = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "what": what,
        "why": why,
        "decision": decision,
        "detail": detail.chars().take(400).collect::<String>(),
    });
    let _guard = AUDIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
    {
        let _ = f.write_all(format!("{record}\n").as_bytes());
    }
}

/// Serializes audit appends (parallel tool batches write at the same time).
static AUDIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn denied_error(what: &str, why: &str, detail: &str) -> String {
    let head: String = detail.chars().take(200).collect();
    format!(
        "BLOCKED ({why}) — the user did not approve this {what} action, so it did NOT run. \
         Payload: {head} \
         Do not retry it through another tool or interpreter; tell the user what you \
         wanted to do and why, then wait for instructions."
    )
}

fn headless_error(what: &str, why: &str) -> String {
    format!(
        "BLOCKED ({why}): this {what} action needs the user's confirmation and there is \
         no interactive channel in this run, so it is denied by default. Ask the user."
    )
}

fn confirm_message(what: &str, detail: &str, why: &str) -> String {
    let body: String = detail.chars().take(400).collect();
    format!("{what} — destructive ({why})\n\n{body}\n\nAllow this?")
}

/// Sends a prompt to the TUI and waits for the answer
/// (y/Enter = yes, n/Esc = no). `None` = there is no interactive channel.
/// Sync variant for tools on the blocking pool.
pub fn dialog_blocking(state: &AppState, message: String) -> Option<bool> {
    let sender = {
        let ui_tx = state.ui_tx.lock().unwrap_or_else(|e| e.into_inner());
        ui_tx.clone()
    };
    let tx = sender?;
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel::<bool>();
    let _ = tx.send(UiEvent::ConfirmAction {
        message,
        tx: Arc::new(resp_tx),
    });
    resp_rx.blocking_recv().ok()
}

/// Like `dialog_blocking`, without blocking the thread (for async tools).
pub async fn dialog_async(state: &AppState, message: String) -> Option<bool> {
    let sender = {
        let ui_tx = state.ui_tx.lock().unwrap_or_else(|e| e.into_inner());
        ui_tx.clone()
    };
    let tx = sender?;
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel::<bool>();
    let _ = tx.send(UiEvent::ConfirmAction {
        message,
        tx: Arc::new(resp_tx),
    });
    resp_rx.await.ok()
}

/// Rule 2 (test/headless path): the USER approved in advance for this
/// process. The env variable is never read from the payload.
pub fn env_approved() -> bool {
    std::env::var("RIJO_APPROVE_DANGEROUS").as_deref() == Ok("1")
}

/// Blocks until the user answers in the TUI. For SYNC tools that run on the
/// blocking pool (terminal, delete_file).
pub fn approve_blocking(
    state: &AppState,
    what: &str,
    detail: &str,
    why: &str,
) -> Result<(), String> {
    if self_approval_attempt(detail) {
        log_decision(what, detail, why, "self-approval-rejected");
        return Err(self_approval_error(what, why));
    }
    if env_approved() {
        log_decision(what, detail, why, "approved-env");
        return Ok(());
    }
    match dialog_blocking(state, confirm_message(what, detail, why)) {
        Some(true) => {
            log_decision(what, detail, why, "approved-by-user");
            Ok(())
        }
        Some(false) => {
            log_decision(what, detail, why, "denied-by-user");
            Err(denied_error(what, why, detail))
        }
        None => {
            log_decision(what, detail, why, "denied-headless");
            Err(headless_error(what, why))
        }
    }
}

/// Like `approve_blocking`, but without blocking the thread — for
/// ASYNC tools (execute_code) that run directly on the runtime.
pub async fn approve_async(
    state: &AppState,
    what: &str,
    detail: &str,
    why: &str,
) -> Result<(), String> {
    if self_approval_attempt(detail) {
        log_decision(what, detail, why, "self-approval-rejected");
        return Err(self_approval_error(what, why));
    }
    if env_approved() {
        log_decision(what, detail, why, "approved-env");
        return Ok(());
    }
    match dialog_async(state, confirm_message(what, detail, why)).await {
        Some(true) => {
            log_decision(what, detail, why, "approved-by-user");
            Ok(())
        }
        Some(false) => {
            log_decision(what, detail, why, "denied-by-user");
            Err(denied_error(what, why, detail))
        }
        None => {
            log_decision(what, detail, why, "denied-headless");
            Err(headless_error(what, why))
        }
    }
}

/// Does the command execute a script file whose CONTENT is destructive?
///
/// `bash cleanup.sh` does not contain a pattern itself — the payload is in the
/// file. Therefore the first existing interpreter argument path
/// is read and checked (max. 64 KB).
pub fn script_file_reason(
    command: &str,
    workspace: &std::path::Path,
) -> Option<(String, &'static str)> {
    const INTERPRETERS: &[&str] = &[
        "bash", "sh", "zsh", "dash", "python", "python3", "node", "perl", "ruby",
    ];
    let mut parts = command.split_whitespace();
    let first = parts.next()?;
    let first = first.rsplit('/').next().unwrap_or("");
    if !INTERPRETERS.contains(&first) {
        return None;
    }
    for arg in parts {
        if arg.starts_with('-') {
            continue;
        }
        let candidate = std::path::Path::new(arg);
        let path = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            workspace.join(candidate)
        };
        if !path.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text: String =
            String::from_utf8_lossy(&bytes[..bytes.len().min(64 * 1024)]).to_string();
        if let Some(why) = command_reason(&text).or_else(|| code_reason(&text)) {
            return Some((path.display().to_string(), why));
        }
        return None;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_patterns_cover_the_real_incident() {
        assert_eq!(command_reason("rm -rf -- ./*"), Some("recursive delete"));
        assert_eq!(
            command_reason("shopt -s dotglob; rm -fr /tmp/x"),
            Some("recursive delete")
        );
        assert_eq!(command_reason("rm -r target"), Some("recursive delete"));
        assert_eq!(
            command_reason("find . -name '*.tmp' -delete"),
            Some("recursive delete")
        );
        assert_eq!(
            command_reason("git reset --hard HEAD~3"),
            Some("history rewrite")
        );
        assert_eq!(
            command_reason("dd if=/dev/zero of=/dev/sda"),
            Some("disk dump/write")
        );
    }

    #[test]
    fn normal_commands_are_not_flagged() {
        for cmd in [
            "cargo test",
            "ls -la",
            "git status --short",
            "cargo clippy -- -D warnings",
            "python3 -m pytest -q",
            "find . -name '*.rs' | head",
            "mkdir -p build && touch build/x",
        ] {
            assert_eq!(command_reason(cmd), None, "{cmd} must not be blocked");
        }
    }

    /// Windows destructive commands were not in the list at all until
    /// 2026-10-29 — the app ships on Windows first.
    #[test]
    fn windows_destructive_commands_are_recognised() {
        for (cmd, why) in [
            (r"del /f /s /q C:\tmp\out", "force delete"),
            (r"rd /s /q build", "recursive delete"),
            (r"rmdir /s /q build", "recursive delete"),
            (r"Remove-Item -Recurse -Force .\out", "recursive delete"),
            ("format c: /q", "filesystem format"),
            ("diskpart", "disk partitioning"),
            (r"reg delete HKCU\Software\X /f", "registry delete"),
        ] {
            assert_eq!(command_reason(cmd), Some(why), "{cmd}");
        }
    }

    /// Short patterns must start a command: `model /f` is not `del /f`, and
    /// `word /s` is not `rd /s`.
    #[test]
    fn short_windows_patterns_do_not_fire_inside_words() {
        assert_eq!(command_reason("python -m model /f"), None);
        assert_eq!(command_reason("grep word /s docs"), None);
        // A shell token after a quote still counts (the documented case).
        assert_eq!(
            command_reason("powershell -c \"del /f x\""),
            Some("force delete")
        );
    }

    /// The heuristic works on text, not on effect: a `grep` AFTER a
    /// destructive pattern is blocked as well. Deliberately accepted — a
    /// false alarm costs one confirmation, a miss costs data.
    #[test]
    fn text_heuristic_has_a_documented_false_positive() {
        assert_eq!(
            command_reason("grep -rn 'rm -rf' README.md"),
            Some("recursive delete")
        );
    }

    #[test]
    fn code_payloads_are_caught() {
        // The real bypass route: python instead of shell.
        assert_eq!(
            code_reason("import shutil; shutil.rmtree('/tmp/x')"),
            Some("recursive delete")
        );
        assert_eq!(
            code_reason("import os\nos.remove('a.txt')"),
            Some("delete file")
        );
        assert_eq!(
            code_reason("from pathlib import Path; Path('d').rmdir()"),
            Some("remove directory")
        );
        assert_eq!(
            code_reason("import os; os.system('rm -rf /tmp/x')"),
            Some("recursive delete")
        );
        assert_eq!(
            code_reason("fs.rmSync('x', {recursive: true})"),
            Some("recursive delete")
        );
    }

    #[test]
    fn harmless_code_is_not_flagged() {
        for code in [
            "import json; print(json.dumps({'a': 1}))",
            "print(open('README.md').read()[:100])",
            "from hermes_tools import web_search; print(web_search('rust'))",
            "import pathlib; print(list(pathlib.Path('.').iterdir()))",
        ] {
            assert_eq!(code_reason(code), None, "{code} must not be blocked");
        }
    }

    #[test]
    fn self_approval_is_detected() {
        assert!(self_approval_attempt(
            "RIJO_APPROVE_DANGEROUS=1 rm -rf /tmp/x"
        ));
        assert!(self_approval_attempt("export RIJO_APPROVE_DANGEROUS=1"));
        assert!(self_approval_attempt(
            "import os; os.environ['rijo_approve_dangerous']='1'"
        ));
        assert!(!self_approval_attempt("rm -rf /tmp/x"));
    }

    #[test]
    fn script_content_is_inspected() {
        let dir = std::env::temp_dir().join("rijo_guard_test");
        let _ = std::fs::create_dir_all(&dir);
        let bad = dir.join("cleanup.sh");
        std::fs::write(&bad, "#!/bin/sh\nrm -rf \"$HOME/tmp\"\n").unwrap();
        let good = dir.join("build.sh");
        std::fs::write(&good, "#!/bin/sh\ncargo build\n").unwrap();

        assert!(script_file_reason("bash cleanup.sh", &dir).is_some());
        assert!(script_file_reason("sh cleanup.sh", &dir).is_some());
        assert!(script_file_reason("bash build.sh", &dir).is_none());
        // No interpreter -> not our business here (the command gate covers
        // that, e.g. `rm -rf` directly).
        assert!(script_file_reason("cargo build", &dir).is_none());
    }
}
