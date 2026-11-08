//! `git` — read-only inspection of the local repository.
//!
//! Why a tool: every repo analysis used to shell out for `git status`,
//! `git log`, `git diff` and `git ls-files`. Those calls cost a process, a shell
//! and a large unbounded text result, and they were the source of two failure
//! modes seen on 2026-10-17 (a 180 s hang in a `git ls-files | xargs grep`
//! pipeline, and a run that analysed the launch directory instead of the
//! workspace). This tool runs git directly, always inside the configured
//! workspace, with a timeout, closed stdin and capped output.
//!
//! Strictly read-only: no action can change refs, the index or the worktree.

use crate::tools::{tool_error, JsonSchema, Tool, ToolMeta, ToolResult};
use crate::AppState;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub struct GitTool;

/// Upper bounds: a git result must never flood the model's context.
const MAX_LIST: usize = 200;
const MAX_FILES: usize = 300;
const MAX_TEXT_CHARS: usize = 40_000;
const TIMEOUT: Duration = Duration::from_secs(20);

struct GitOutput {
    stdout: String,
    stderr: String,
    ok: bool,
}

fn run_git(workspace: &Path, args: &[String]) -> Result<GitOutput, String> {
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(workspace)
        .arg("--no-pager")
        .args(args)
        // Never prompt, never page: both would wait for input that an agent
        // command cannot provide.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // The desktop binary is a GUI process; without this every git call
        // would flash a console window.
        command.creation_flags(0x0800_0000);
    }

    let child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "git is not installed or not on PATH".to_string()
        } else {
            format!("cannot run git: {e}")
        }
    })?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    // Reader in a thread: `wait_with_output` drains the pipes, and the timeout
    // below still fires if git blocks.
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(output)) => Ok(GitOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            ok: output.status.success(),
        }),
        Ok(Err(e)) => Err(format!("git failed: {e}")),
        Err(_) => {
            crate::process::kill_tree(pid);
            Err(format!(
                "git timed out after {}s (the tree was killed)",
                TIMEOUT.as_secs()
            ))
        }
    }
}

fn cap_text(text: &str, limit: usize) -> (String, bool) {
    if text.chars().count() <= limit {
        return (text.to_string(), false);
    }
    let mut capped: String = text.chars().take(limit).collect();
    capped.push_str("\n…[truncated]");
    (capped, true)
}

fn capped_list(items: Vec<String>) -> (Vec<String>, bool) {
    if items.len() <= MAX_LIST {
        return (items, false);
    }
    (items.into_iter().take(MAX_LIST).collect(), true)
}

fn trimmed_error(stderr: &str, stdout: &str) -> String {
    let message = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let (text, _) = cap_text(message.trim(), 2_000);
    text
}

/// Parse `git status --porcelain=v1 -b --untracked-files=all`.
fn parse_status(text: &str) -> Value {
    let mut branch = String::new();
    let mut upstream = String::new();
    let (mut ahead, mut behind) = (0u32, 0u32);
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();

    for line in text.lines() {
        if let Some(head) = line.strip_prefix("## ") {
            let (names, flags) = match head.split_once(" [") {
                Some((names, flags)) => (names, Some(flags)),
                None => (head, None),
            };
            match names.split_once("...") {
                Some((local, remote)) => {
                    branch = local.trim().to_string();
                    upstream = remote.trim().to_string();
                }
                None => branch = names.trim().to_string(),
            }
            if let Some(flags) = flags {
                for part in flags.trim_end_matches(']').split(',') {
                    let part = part.trim();
                    if let Some(value) = part.strip_prefix("ahead ") {
                        ahead = value.parse().unwrap_or(0);
                    }
                    if let Some(value) = part.strip_prefix("behind ") {
                        behind = value.parse().unwrap_or(0);
                    }
                }
            }
            continue;
        }
        // `XY path`, two status columns and a space. Paths may be non-ASCII,
        // so slice on the known ASCII prefix.
        if line.len() < 4 {
            continue;
        }
        let bytes = line.as_bytes();
        let (x, y) = (bytes[0] as char, bytes[1] as char);
        let path = line[3..].to_string();
        if x == '?' && y == '?' {
            untracked.push(path);
        } else {
            if x != ' ' {
                staged.push(path.clone());
            }
            if y != ' ' {
                unstaged.push(path);
            }
        }
    }

    let (staged, staged_truncated) = capped_list(staged);
    let (unstaged, unstaged_truncated) = capped_list(unstaged);
    let (untracked, untracked_truncated) = capped_list(untracked);
    json!({
        "branch": if branch.is_empty() { Value::Null } else { json!(branch) },
        "upstream": if upstream.is_empty() { Value::Null } else { json!(upstream) },
        "ahead": ahead,
        "behind": behind,
        "clean": staged.is_empty() && unstaged.is_empty() && untracked.is_empty(),
        "counts": {
            "staged": staged.len(),
            "unstaged": unstaged.len(),
            "untracked": untracked.len()
        },
        "staged": staged,
        "unstaged": unstaged,
        "untracked": untracked,
        "truncated": staged_truncated || unstaged_truncated || untracked_truncated
    })
}

/// Parse the `%h %ad %an %s` log format, tab-separated.
fn parse_log(text: &str, limit: usize) -> Vec<Value> {
    text.lines()
        .take(limit)
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let hash = parts.next()?;
            let date = parts.next()?;
            let author = parts.next()?;
            let subject = parts.next().unwrap_or("");
            Some(json!({
                "hash": hash,
                "date": date,
                "author": author,
                "subject": subject
            }))
        })
        .collect()
}

fn parse_branches(text: &str) -> Value {
    let mut current = String::new();
    let mut branches = Vec::new();
    for line in text.lines() {
        let mut parts = line.split('\t');
        let marker = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("");
        let upstream = parts.next().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        if marker == "*" {
            current = name.to_string();
        }
        branches.push(json!({
            "name": name,
            "current": marker == "*",
            "upstream": if upstream.is_empty() { Value::Null } else { json!(upstream) }
        }));
    }
    json!({
        "current": if current.is_empty() { Value::Null } else { json!(current) },
        "branches": branches
    })
}

impl Tool for GitTool {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "git",
            description: "Inspect the local git repository (read-only): status, log, diff, show, branches, tracked files. Use this instead of shell `git` calls.",
            emoji: "🌿",
            toolset: "git",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &GIT_SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let action = match args.get("action").and_then(|v| v.as_str()) {
            Some(action) => action,
            None => return tool_error("action is required"),
        };
        let path = args.get("path").and_then(|v| v.as_str());
        let reference = args.get("ref").and_then(|v| v.as_str());
        let staged = args
            .get("staged")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let stat = args.get("stat").and_then(|v| v.as_bool()).unwrap_or(false);
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(15)
            .clamp(1, 50) as usize;
        let workspace = state.config.workspace.clone();

        let mut git_args: Vec<String> = Vec::new();
        match action {
            "status" => {
                git_args = ["status", "--porcelain=v1", "-b", "--untracked-files=all"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
            }
            "log" => {
                git_args = vec![
                    "log".to_string(),
                    format!("-n{limit}"),
                    "--date=short".to_string(),
                    "--pretty=format:%h%x09%ad%x09%an%x09%s".to_string(),
                ];
                if let Some(reference) = reference {
                    git_args.push(reference.to_string());
                }
            }
            "diff" => {
                git_args.push("diff".to_string());
                if staged {
                    git_args.push("--staged".to_string());
                }
                if stat {
                    git_args.push("--stat".to_string());
                }
                git_args.push("--no-color".to_string());
                if let Some(reference) = reference {
                    git_args.push(reference.to_string());
                }
                git_args.push("--".to_string());
                if let Some(path) = path {
                    git_args.push(path.to_string());
                }
            }
            "show" => {
                git_args.push("show".to_string());
                git_args.push("--no-color".to_string());
                if stat {
                    git_args.push("--stat".to_string());
                } else {
                    git_args.push("--patch".to_string());
                }
                git_args.push("--format=fuller".to_string());
                git_args.push(reference.unwrap_or("HEAD").to_string());
                git_args.push("--".to_string());
                if let Some(path) = path {
                    git_args.push(path.to_string());
                }
            }
            "branch" => {
                git_args = [
                    "for-each-ref",
                    "--format=%(HEAD)%09%(refname:short)%09%(upstream:short)",
                    "refs/heads",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
            }
            "files" => {
                git_args.push("ls-files".to_string());
                if let Some(path) = path {
                    git_args.push("--".to_string());
                    git_args.push(path.to_string());
                }
            }
            other => return tool_error(format!("unknown action: {other}")),
        }

        let output = match run_git(&workspace, &git_args) {
            Ok(output) => output,
            Err(error) => return tool_error(error),
        };
        if !output.ok {
            return tool_error(format!(
                "git {action} failed: {}",
                trimmed_error(&output.stderr, &output.stdout)
            ));
        }

        match action {
            "status" => parse_status(&output.stdout).to_string(),
            "log" => json!({ "commits": parse_log(&output.stdout, limit) }).to_string(),
            "diff" => {
                let (diff, truncated) = cap_text(&output.stdout, MAX_TEXT_CHARS);
                json!({
                    "staged": staged,
                    "ref": reference,
                    "diff": diff,
                    "truncated": truncated
                })
                .to_string()
            }
            "show" => {
                let (patch, truncated) = cap_text(&output.stdout, MAX_TEXT_CHARS);
                json!({
                    "ref": reference.unwrap_or("HEAD"),
                    "patch": patch,
                    "truncated": truncated
                })
                .to_string()
            }
            "branch" => parse_branches(&output.stdout).to_string(),
            "files" => {
                let all: Vec<String> = output
                    .stdout
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| line.to_string())
                    .collect();
                let count = all.len();
                let (files, truncated) = if count > MAX_FILES {
                    (all.into_iter().take(MAX_FILES).collect::<Vec<_>>(), true)
                } else {
                    (all, false)
                };
                json!({ "count": count, "files": files, "truncated": truncated }).to_string()
            }
            _ => tool_error("unknown action"),
        }
    }
}

lazy_static::lazy_static! {
    static ref GIT_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["status", "log", "diff", "show", "branch", "files"],
                "description": "status = branch, ahead/behind, changed files; log = recent commits; diff = working tree (or staged / vs ref); show = one commit; branch = local branches; files = tracked files"
            },
            "path": {
                "type": "string",
                "description": "Optional path filter, relative to the workspace (diff, show, files)"
            },
            "ref": {
                "type": "string",
                "description": "Commit or branch for diff/show (default: working tree / HEAD)"
            },
            "staged": {
                "type": "boolean",
                "description": "diff: show staged changes instead of the working tree"
            },
            "stat": {
                "type": "boolean",
                "description": "diff/show: summary only, no patch"
            },
            "limit": {
                "type": "integer",
                "description": "log: number of commits (default 15, max 50)"
            }
        },
        "required": ["action"]
    }));
}
#[cfg(test)]
mod tests {
    use super::{cap_text, parse_branches, parse_log, parse_status};

    const STATUS: &str =
        "## main...origin/main [ahead 2, behind 1]\n M src/a.rs\nM  src/b.rs\n?? notes.txt\n";

    #[test]
    fn status_is_parsed_into_branch_counts_and_lists() {
        let value = parse_status(STATUS);
        assert_eq!(value["branch"], "main");
        assert_eq!(value["upstream"], "origin/main");
        assert_eq!(value["ahead"], 2);
        assert_eq!(value["behind"], 1);
        assert_eq!(value["clean"], false);
        assert_eq!(value["counts"]["staged"], 1);
        assert_eq!(value["counts"]["unstaged"], 1);
        assert_eq!(value["counts"]["untracked"], 1);
        assert_eq!(value["staged"][0], "src/b.rs");
        assert_eq!(value["unstaged"][0], "src/a.rs");
        assert_eq!(value["untracked"][0], "notes.txt");
    }

    #[test]
    fn a_clean_tree_is_reported_as_clean() {
        let value = parse_status("## main\n");
        assert_eq!(value["clean"], true);
        assert_eq!(value["counts"]["staged"], 0);
        assert!(value["branch"] == "main");
    }

    #[test]
    fn log_lines_become_commit_objects() {
        let text =
            "abc1234\t2026-10-17\tMo\tAdd the git tool\n\ndef5678\t2026-10-16\tMo\tFix stdin\n";
        let commits = parse_log(text, 15);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["hash"], "abc1234");
        assert_eq!(commits[0]["date"], "2026-10-17");
        assert_eq!(commits[0]["author"], "Mo");
        assert_eq!(commits[0]["subject"], "Add the git tool");
    }

    #[test]
    fn branches_mark_the_current_one() {
        let value = parse_branches("*\tmain\torigin/main\n\tfeature\t\n");
        assert_eq!(value["current"], "main");
        assert_eq!(value["branches"][0]["current"], true);
        assert_eq!(value["branches"][1]["name"], "feature");
        assert_eq!(value["branches"][1]["current"], false);
    }

    #[test]
    fn long_results_are_capped_and_flagged() {
        let long = "x".repeat(80);
        let (text, truncated) = cap_text(&long, 40);
        assert!(truncated);
        assert!(text.chars().count() < 80);
        assert!(text.ends_with("[truncated]"));

        let (text, truncated) = cap_text("short", 40);
        assert!(!truncated);
        assert_eq!(text, "short");
    }
}
