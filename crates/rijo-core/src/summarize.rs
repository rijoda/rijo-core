//! Human one-line summaries of tool calls.
//!
//! The core owns this wording so every frontend shows the same target instead
//! of each parsing raw JSON on its own (core/UI boundary). The executor uses
//! [`tool_call`] for `ProgressEvent::ToolCall`; the TUI and the desktop render
//! that string directly.

use serde_json::Value;

/// One-line target of a tool call: `terminal` → `cargo`, `read_file` → `lib.rs`.
///
/// Returns an empty string when the arguments carry no useful target (unknown
/// tool, missing key, invalid JSON). Callers decide whether to fall back to the
/// raw arguments; an empty result is never an error.
pub fn tool_call(tool: &str, args_json: &str) -> String {
    let args: Value = serde_json::from_str(args_json).unwrap_or(Value::Null);
    let host_of = |u: &str| -> String {
        u.split("//")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or(u)
            .to_string()
    };
    let summary = match tool {
        // file-targeting tools: show the path (basename is enough)
        "read_file" | "write_file" | "patch" | "delete_file" => {
            let p = get_value(&args, &["path", "file_path", "file"]);
            if p.is_empty() {
                String::new()
            } else {
                basename(&p)
            }
        }
        // terminal - the real command; mark background clearly
        "terminal" => {
            let c = get_value(&args, &["command"]);
            let mut command = c.split_whitespace().collect::<Vec<_>>().join(" ");
            // `cd <path> && real-command` is boilerplate (run in the workspace).
            // Showing "cd" told the user nothing - show the real command instead.
            if command.starts_with("cd ") {
                let rest = command
                    .rsplit_once("&&")
                    .map(|(_, r)| r)
                    .or_else(|| command.rsplit_once(';').map(|(_, r)| r))
                    .unwrap_or("");
                command = rest.trim().to_string();
            }
            if args
                .get("background")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
            {
                if command.is_empty() {
                    "bg".to_string()
                } else {
                    format!("{command} (bg)")
                }
            } else {
                command
            }
        }
        // background process: action + target id - that is the relevant info
        "process_manage" => {
            let a = get_value(&args, &["action"]);
            let id = get_value(&args, &["session_id"]);
            match (a.is_empty(), id.is_empty()) {
                (false, false) => format!("{a} {}", short_id(&id)),
                (false, true) => a,
                _ => String::new(),
            }
        }
        // search tools: pattern/query
        "search_files" | "web_search" | "session_search" => {
            let k = get_value(&args, &["pattern", "query"]);
            if k.is_empty() {
                String::new()
            } else {
                format!("'{k}'")
            }
        }
        "arxiv" => {
            let action = get_value(&args, &["action"]);
            let target = get_value(&args, &["query", "id"]);
            match (action.is_empty(), target.is_empty()) {
                (false, false) => format!("{action} '{target}'"),
                (false, true) => action,
                _ => target,
            }
        }
        "brain_search" => get_value(&args, &["query"]).chars().take(48).collect(),
        // clarify: the questions are the content; the raw JSON is not readable
        "clarify" => {
            let questions = args.get("questions").and_then(|v| v.as_array());
            let count = questions.map(|list| list.len()).unwrap_or(0);
            match count {
                0 => String::new(),
                1 => questions
                    .and_then(|list| list.first())
                    .and_then(|question| question.get("question"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("1 question")
                    .chars()
                    .take(60)
                    .collect(),
                n => format!("{n} questions"),
            }
        }
        "git" => {
            let action = get_value(&args, &["action"]);
            let path = get_value(&args, &["path"]);
            if path.is_empty() {
                action
            } else {
                format!("{action} {path}")
            }
        }
        "mcp" => {
            let action = get_value(&args, &["action"]);
            let server = get_value(&args, &["server"]);
            let tool = get_value(&args, &["tool"]);
            match (action.as_str(), server.is_empty(), tool.is_empty()) {
                ("call", false, false) => format!("{server}/{tool}"),
                (_, false, _) => format!("{action} {server}"),
                _ => action,
            }
        }
        // web_extract: the host of the first URL is more informative than the whole URL
        "web_extract" => {
            let u = args
                .get("urls")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .or_else(|| args.get("url").and_then(|v| v.as_str()))
                .unwrap_or("");
            if u.is_empty() {
                String::new()
            } else {
                host_of(u)
            }
        }
        // context: the action plus what it is about
        "context" => {
            let action = get_value(&args, &["action"]);
            let query = get_value(&args, &["query"]);
            match (action.is_empty(), query.is_empty()) {
                (false, false) => format!("{action} '{query}'"),
                (false, true) => action,
                _ => query,
            }
        }
        // github: the action is the verb, the repository/PR the object
        "github" => {
            let action = get_value(&args, &["action"]);
            // 'number' is a JSON integer, get_value only reads strings.
            let number = args
                .get("number")
                .and_then(|v| v.as_i64())
                .map(|n| format!("#{n}"))
                .unwrap_or_default();
            let repo = get_value(&args, &["repo", "name"]);
            let short_repo = repo.rsplit('/').next().unwrap_or("").to_string();
            let target = if !number.is_empty() {
                number
            } else {
                short_repo
            };
            match (action.is_empty(), target.is_empty()) {
                (false, false) => format!("{action} {target}"),
                (false, true) => action,
                _ => target,
            }
        }
        // skills: the name is what you are looking for
        "skill_view" | "skill_manage" => get_value(&args, &["name"]),
        "skills_list" => get_value(&args, &["category"]),
        // browser: target URL or selector
        "browser_navigate" => {
            let u = get_value(&args, &["url"]);
            if u.is_empty() {
                String::new()
            } else {
                host_of(&u)
            }
        }
        "browser_click" | "browser_type" => {
            let sel = get_value(&args, &["ref", "selector"]);
            let text = get_value(&args, &["text"]);
            if tool == "browser_type" && !text.is_empty() {
                let t: String = text.chars().take(20).collect();
                format!("{sel} → {t}")
            } else {
                sel
            }
        }
        "browser_scroll" => get_value(&args, &["direction"]),
        "browser_dev" => {
            let action = get_value(&args, &["action"]);
            let target = get_value(&args, &["url", "id", "name", "expression"]);
            match (action.is_empty(), target.is_empty()) {
                (false, false) => {
                    format!("{action} {}", target.chars().take(40).collect::<String>())
                }
                (false, true) => action,
                _ => String::new(),
            }
        }
        // memory / cron / todo: action or target
        "memory" => {
            let a = get_value(&args, &["action"]);
            let t = get_value(&args, &["target"]);
            if a.is_empty() {
                t
            } else if t.is_empty() {
                a
            } else {
                format!("{a}/{t}")
            }
        }
        "cronjob_manage" => get_value(&args, &["action", "name"]),
        "todo_list" => {
            let n = args
                .get("todos")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            if n > 0 {
                // The raw count duplicated the result line, which now reports the
                // resulting state. Say what the call DOES instead.
                "update".to_string()
            } else {
                String::new()
            }
        }
        "execute_code" => "python".to_string(),
        "vision_analyze" => {
            let p = get_value(&args, &["image_url", "path"]);
            if p.is_empty() {
                String::new()
            } else {
                basename(&p)
            }
        }
        "text_to_speech" => get_value(&args, &["voice"]),
        box_tool if box_tool.starts_with("box_") => {
            let target = get_value(&args, &["path", "cmd", "url", "box_id"]);
            if target.contains('/') && !target.starts_with("http") {
                basename(&target)
            } else {
                target.chars().take(40).collect()
            }
        }
        "delegate_task" => {
            let g = get_value(&args, &["goal", "task"]);
            g.chars().take(30).collect()
        }
        _ => String::new(),
    };
    // A pathological value (a whole command line, a pasted prompt) must not
    // become an unbounded label.
    summary.chars().take(80).collect()
}

/// `proc_00000001` → `proc_1`: the id is zero-padded, the last digits
/// are the information - a prefix cut makes all ids look the same.
pub fn short_id(id: &str) -> String {
    if let Some(rest) = id.strip_prefix("proc_") {
        let digits = rest.trim_start_matches('0');
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            return format!("proc_{digits}");
        }
    }
    id.chars().take(12).collect()
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Fetch the first non-empty string value from candidate keys.
fn get_value(args: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(s) = args.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_names_the_target() {
        let cases: Vec<(&str, &str, &str)> = vec![
            (
                "process_manage",
                r#"{"action":"wait","session_id":"proc_00000001"}"#,
                "wait proc_1",
            ),
            (
                "terminal",
                r#"{"command":"cargo build","background":true}"#,
                "cargo build (bg)",
            ),
            (
                "web_extract",
                r#"{"urls":["https://doc.rust-lang.org/book/x.html"]}"#,
                "doc.rust-lang.org",
            ),
            (
                "arxiv",
                r#"{"action":"search","query":"attention is all you need"}"#,
                "search 'attention is all you need'",
            ),
            (
                "brain_search",
                r#"{"query":"tool registry"}"#,
                "tool registry",
            ),
            (
                "mcp",
                r#"{"action":"call","server":"docs","tool":"search"}"#,
                "docs/search",
            ),
            (
                "browser_navigate",
                r#"{"url":"https://example.org/a"}"#,
                "example.org",
            ),
            (
                "skill_view",
                r#"{"name":"rijo-rust-harness"}"#,
                "rijo-rust-harness",
            ),
            ("execute_code", r#"{"code":"x"}"#, "python"),
            ("session_search", r#"{"query":"websurfx"}"#, "'websurfx'"),
            ("memory", r#"{"action":"add","target":"user"}"#, "add/user"),
            (
                // A leading `cd <path> &&` is boilerplate; the keyword must be the
                // real command, not "cd".
                "terminal",
                r#"{"command":"cd /tmp/ws && grep -n foo ."}"#,
                "grep -n foo .",
            ),
            ("terminal", r#"{"command":"cargo test"}"#, "cargo test"),
            (
                "github",
                r#"{"action":"repo_view","repo":"rijoda/rijo-code"}"#,
                "repo_view rijo-code",
            ),
            (
                "github",
                r#"{"action":"pr_view","repo":"rijoda/rijo-code","number":7}"#,
                "pr_view #7",
            ),
            (
                "github",
                r#"{"action":"repo_create","name":"rijo-sync"}"#,
                "repo_create rijo-sync",
            ),
            (
                "read_file",
                r#"{"path":"crates/rijo-core/src/lib.rs"}"#,
                "lib.rs",
            ),
            (
                "clarify",
                r#"{"questions":[{"id":"a","question":"Which file?"}]}"#,
                "Which file?",
            ),
            (
                "clarify",
                r#"{"questions":[{"id":"a","question":"A?"},{"id":"b","question":"B?"}]}"#,
                "2 questions",
            ),
            ("browser_dev", r#"{"action":"screenshot"}"#, "screenshot"),
            (
                "browser_dev",
                r#"{"action":"evaluate","expression":"document.title"}"#,
                "evaluate document.title",
            ),
            ("box_fs_read", r#"{"path":"src/lib.rs"}"#, "lib.rs"),
        ];
        for (tool, args, want) in cases {
            assert_eq!(tool_call(tool, args), want, "{tool}");
        }
    }

    #[test]
    fn an_unknown_tool_or_broken_json_has_no_target() {
        assert_eq!(tool_call("mystery_tool", r#"{"x":1}"#), "");
        assert_eq!(tool_call("read_file", "not json"), "");
        assert_eq!(tool_call("read_file", "{}"), "");
    }

    #[test]
    fn a_long_target_is_capped() {
        let long = "x".repeat(200);
        let args = format!(r#"{{"command":"{long}"}}"#);
        assert_eq!(tool_call("terminal", &args).chars().count(), 80);
    }

    #[test]
    fn short_id_strips_zero_padding_only() {
        assert_eq!(short_id("proc_00000001"), "proc_1");
        assert_eq!(short_id("proc_abc"), "proc_abc");
        assert_eq!(short_id("some-very-long-id"), "some-very-lo");
    }
}
