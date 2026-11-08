//! `github` — native GitHub REST client for repository work.
//!
//! Why a tool instead of shelling out to the official `gh` CLI: every `gh`
//! invocation pays a process spawn, its config parsing and a keyring read, and
//! the caller has to parse prose meant for humans.
//!
//! Measured on this machine (2026-09-16, warm cache, network bound):
//!   `gh --help` (no network)                        93-96 ms
//!   `gh auth token` (keyring read)                  114-192 ms
//!   `gh repo view`                                  631-906 ms  (median 809)
//!   pure REST round trip to api.github.com          432-679 ms  (median 597)
//!   this tool, first call in a process              1784 ms  (token discovery +
//!                                                             TLS + round trip)
//!   this tool, later call in the same process        710 ms  (network floor)
//!   this tool, repeated read inside the cache TTL     ~0 ms   (no HTTP request)
//!
//! So the honest claim is: the process start and the keyring read disappear
//! (~200 ms per call), the pooled connection reaches the round-trip floor, and
//! a repeated read inside a turn is free. GitHub's round trip itself does not
//! get faster.
//!
//! Auth: the token is taken from the environment (`GH_TOKEN`, `GITHUB_TOKEN`) or,
//! if absent, from `gh auth token` (the gh keyring — no token ever needs to be
//! written into Rijo Code's config). The token is never logged, never echoed into a
//! tool result and never accepted as a tool argument.
//!
//! Writes that leave a trace on GitHub go through the approval gate
//! (`repo_delete`, `pr_merge`, `release_create`, and `repo_create` when it would
//! create a PUBLIC repository). Read actions and reversible writes (issue,
//! comment, PR create) are free.

use crate::tools::{tool_error, JsonSchema, ToolMeta, ToolResult};
use crate::AppState;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct GithubTool;

const DEFAULT_API_BASE: &str = "https://api.github.com";
const USER_AGENT: &str = "rijo-agent";
/// Raw `api` responses are truncated: the model asked for the payload, but it
/// still has to fit in a context window.
const RAW_LIMIT: usize = 4000;
lazy_static::lazy_static! {
    static ref GITHUB_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "description": "What to do. Reads: auth, repo_view, repo_list, branch_list, commit_list, pr_list, pr_view, issue_list, release_list, run_list, search_repos, api. Writes: repo_create, repo_delete, pr_create, pr_merge, issue_create, issue_close, release_create.",
                "enum": [
                    "auth", "repo_view", "repo_list", "repo_create", "repo_delete",
                    "branch_list", "commit_list", "pr_list", "pr_view", "pr_create",
                    "pr_merge", "issue_list", "issue_create", "issue_close",
                    "release_list", "release_create", "run_list", "search_repos", "api"
                ]
            },
            "repo": {
                "type": "string",
                "description": "owner/name. Defaults to the git remote of the current workspace."
            },
            "number": {
                "type": "integer",
                "description": "Issue or pull request number."
            },
            "query": {
                "type": "string",
                "description": "Search text (search_repos) or the path for 'api' (e.g. /repos/o/r/labels)."
            },
            "title": { "type": "string", "description": "Title (PR, issue, release)." },
            "body": { "type": "string", "description": "Body text (PR, issue, release notes)." },
            "head": { "type": "string", "description": "Source branch of a pull request." },
            "base": { "type": "string", "description": "Target branch of a pull request (default: the repo's default branch)." },
            "state": { "type": "string", "description": "Filter: open | closed | all (default open)." },
            "name": { "type": "string", "description": "Repository name for repo_create." },
            "private": { "type": "boolean", "description": "repo_create: private repository (default true). A public one needs approval." },
            "description": { "type": "string", "description": "Description (repo_create)." },
            "tag": { "type": "string", "description": "Tag name (release_create)." },
            "per_page": { "type": "integer", "description": "How many entries for list actions (default 10, max 50)." },
            "org": { "type": "string", "description": "Owner for repo_list (default: the authenticated user)." },
            "method": { "type": "string", "description": "For 'api' only: GET (default) — writes use the named actions." },
            "raw": { "type": "boolean", "description": "Return the raw JSON body instead of the summary (list/view actions)." }
        },
        "required": ["action"]
    }));
}

// ── token resolution ───────────────────────────────────────────────────────

lazy_static::lazy_static! {
    /// `gh auth token` is spawned at most once per process.
    static ref GH_CLI_TOKEN: Mutex<Option<Option<String>>> = Mutex::new(None);
    static ref CLIENT: reqwest::Client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    /// Short-lived read cache: the model often asks the same question twice in a
    /// turn (repo_view before pr_list, CI status while polling).
    static ref READ_CACHE: Mutex<HashMap<String, (Instant, String)>> = Mutex::new(HashMap::new());
    /// Last seen rate-limit state, surfaced by `auth` and on low budget.
    static ref RATE_LIMIT: Mutex<Option<(u32, i64)>> = Mutex::new(None);
    /// Workspace git remote, resolved once per workspace path.
    static ref REPO_OF_WORKSPACE: Mutex<HashMap<String, Option<String>>> = Mutex::new(HashMap::new());
}

/// Token sources in order: environment first (CI/cron friendly), then the gh
/// keyring. Never from the tool arguments — a model must not be able to pick
/// credentials, and a token in the transcript would be a leak.
fn resolve_token(state: &AppState, use_gh_cli: bool) -> Result<(String, &'static str), String> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(value) = std::env::var(var) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok((value, "environment"));
            }
        }
    }
    if let Some(custom) = state.config.github.token_env.as_deref() {
        if let Ok(value) = std::env::var(custom) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok((value, "configured env var"));
            }
        }
    }
    if use_gh_cli {
        let mut cached = GH_CLI_TOKEN.lock().unwrap();
        let entry = cached.get_or_insert_with(|| {
            std::process::Command::new("gh")
                .args(["auth", "token"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .filter(|token| !token.is_empty())
        });
        if let Some(token) = entry.clone() {
            return Ok((token, "gh auth token"));
        }
    }
    Err(
        "no GitHub token found. Options: export GITHUB_TOKEN=... , log in with \
         `gh auth login`, or point github.token_env at an environment variable. \
         Rijo Code never stores the token itself."
            .to_string(),
    )
}

/// `git@github.com:owner/repo.git`, `https://github.com/owner/repo(.git)` and
/// the ssh:// form all have to end up as `owner/repo`.
pub fn parse_remote(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let after_host = if let Some(rest) = url.split("github.com").nth(1) {
        rest.trim_start_matches([':', '/']).to_string()
    } else {
        let rest = url.strip_prefix("git@")?;
        // any other host: keep it out of scope
        if !rest.starts_with("github.com") {
            return None;
        }
        rest.split_once(':')?.1.to_string()
    };
    let cleaned = after_host.trim_end_matches(".git").trim_end_matches('/');
    let parts: Vec<&str> = cleaned.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 2 {
        Some(format!("{}/{}", parts[0], parts[1]))
    } else {
        None
    }
}

fn repo_from_workspace(state: &AppState) -> Option<String> {
    let ws = state.config.workspace.display().to_string();
    if let Some(hit) = REPO_OF_WORKSPACE.lock().unwrap().get(&ws) {
        return hit.clone();
    }
    let resolved = std::process::Command::new("git")
        .arg("-C")
        .arg(&state.config.workspace)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| parse_remote(&String::from_utf8_lossy(&out.stdout)));
    REPO_OF_WORKSPACE
        .lock()
        .unwrap()
        .insert(ws, resolved.clone());
    resolved
}

fn cache_ttl(state: &AppState) -> u64 {
    state.config.github.cache_ttl_secs
}

fn cached_get(key: &str, ttl: u64) -> Option<(String, u64)> {
    if ttl == 0 {
        return None;
    }
    let cache = READ_CACHE.lock().unwrap();
    cache.get(key).and_then(|(at, body)| {
        let age = at.elapsed().as_secs();
        (age < ttl).then(|| (body.clone(), age))
    })
}

/// Any write invalidates the read cache: a stale list after creating a PR is
/// exactly the kind of lie the model would then report to the user.
fn invalidate_read_cache() {
    READ_CACHE.lock().unwrap().clear();
}

// ── actions ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Read,
    Write,
}

fn action_kind(action: &str) -> Option<Kind> {
    let read = [
        "auth",
        "repo_view",
        "repo_list",
        "branch_list",
        "commit_list",
        "pr_list",
        "pr_view",
        "issue_list",
        "release_list",
        "run_list",
        "search_repos",
        "api",
    ];
    let write = [
        "repo_create",
        "repo_delete",
        "pr_create",
        "pr_merge",
        "issue_create",
        "issue_close",
        "release_create",
    ];
    if read.contains(&action) {
        Some(Kind::Read)
    } else if write.contains(&action) {
        Some(Kind::Write)
    } else {
        None
    }
}

/// True only for actions that are known reads. Used by the batching logic: an
/// unknown action must not be assumed harmless just because it is not on the
/// write list.
pub fn action_is_read(action: &str) -> bool {
    matches!(action_kind(action), Some(Kind::Read))
}

/// True when the call needs the user's approval: the writes that leave a mark
/// others can see or that cannot be undone. Reversible writes (opening a PR,
/// filing an issue, closing an issue) run without a dialog — asking for every
/// one of them would train the user to approve blindly.
///
/// Note that this is NOT the batching question: every write breaks a read batch
/// (see `action_is_read`), because ordering matters.
pub fn needs_approval(action: &str, args: &Value) -> bool {
    match action {
        // irreversible
        "repo_delete" | "pr_merge" | "release_create" => true,
        // A PRIVATE repository is the default and reversible; a PUBLIC one
        // publishes the code to everyone, so that one asks.
        "repo_create" => args.get("private").and_then(|v| v.as_bool()) == Some(false),
        _ => false,
    }
}

fn approval_reason(action: &str) -> &'static str {
    match action {
        "repo_delete" => "deletes a GitHub repository (irreversible)",
        "pr_merge" => "merges a pull request (changes a shared branch)",
        "release_create" => "publishes a GitHub release",
        _ => "publishes a public repository",
    }
}

fn api_base(state: &AppState) -> String {
    state
        .config
        .github
        .api_base
        .clone()
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
        .trim_end_matches('/')
        .to_string()
}

struct Response {
    status: reqwest::StatusCode,
    body: Value,
}

async fn request(
    token: &str,
    method: reqwest::Method,
    url: &str,
    body: Option<Value>,
) -> Result<Response, String> {
    let mut req = CLIENT
        .request(method, url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("Authorization", format!("Bearer {token}"));
    if let Some(payload) = body {
        req = req.json(&payload);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("GitHub request failed: {e}"))?;
    let status = resp.status();
    let remaining = resp
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok());
    let reset = resp
        .headers()
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    if let Some(remaining) = remaining {
        *RATE_LIMIT.lock().unwrap() = Some((remaining, reset.unwrap_or(0)));
    }
    let text = resp.text().await.unwrap_or_default();
    let body = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
    Ok(Response { status, body })
}

fn rate_hint() -> String {
    match *RATE_LIMIT.lock().unwrap() {
        Some((remaining, _)) if remaining < 10 => {
            format!(" GitHub API budget is low ({remaining} requests left).")
        }
        _ => String::new(),
    }
}

/// Turn an API error into something the model can act on, with the three cases
/// that actually happen: bad token, missing scope, missing resource.
fn error_message(status: reqwest::StatusCode, body: &Value) -> String {
    let message = body
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("no message");
    let detail = body
        .get("errors")
        .and_then(|e| e.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect::<Vec<_>>()
                .join("; ")
        })
        .filter(|s| !s.is_empty())
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    let advice = match status.as_u16() {
        401 => " — the token is invalid or expired; run `gh auth login` or set GITHUB_TOKEN.",
        403 => " — missing permission/scope, or the API rate limit is exhausted.",
        404 => " — not found (private repository without access, or a typo in owner/name).",
        422 => " — the request was rejected by GitHub (validation).",
        _ => "",
    };
    format!(
        "GitHub API error {}: {message}{detail}{advice}",
        status.as_u16()
    )
}

fn short(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("-")
        .to_string()
}

fn day(value: &Value, key: &str) -> String {
    let raw = short(value, key);
    if raw.len() >= 10 {
        raw[..10].to_string()
    } else {
        raw
    }
}

fn summarize_repo(repo: &Value) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{}  [{}{}]\n",
        short(repo, "full_name"),
        if repo
            .get("private")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            "private"
        } else {
            "public"
        },
        if repo
            .get("archived")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            ", archived"
        } else {
            ""
        }
    ));
    if let Some(desc) = repo.get("description").and_then(|v| v.as_str()) {
        if !desc.is_empty() {
            out.push_str(&format!("  about:       {desc}\n"));
        }
    }
    out.push_str(&format!(
        "  default:     {}\n  url:         {}\n  clone:       {}\n  pushed:      {}   created: {}\n  size:        {} KB   issues open: {}   stars: {}\n",
        short(repo, "default_branch"),
        short(repo, "html_url"),
        short(repo, "clone_url"),
        day(repo, "pushed_at"),
        day(repo, "created_at"),
        repo.get("size").and_then(|v| v.as_i64()).unwrap_or(0),
        repo.get("open_issues_count").and_then(|v| v.as_i64()).unwrap_or(0),
        repo.get("stargazers_count").and_then(|v| v.as_i64()).unwrap_or(0),
    ));
    if let Some(perms) = repo.get("permissions") {
        out.push_str(&format!(
            "  permissions: push={} admin={}\n",
            perms.get("push").and_then(|v| v.as_bool()).unwrap_or(false),
            perms
                .get("admin")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        ));
    }
    out
}

fn summarize_list(items: &[Value], kind: &str) -> String {
    if items.is_empty() {
        return format!("no {kind} found\n");
    }
    let mut out = format!("{} {kind}:\n", items.len());
    for item in items {
        let line = match kind {
            "repository" => format!(
                "  {}{}  ({} , pushed {})",
                short(item, "full_name"),
                if item
                    .get("private")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    " [private]"
                } else {
                    ""
                },
                day(item, "updated_at"),
                day(item, "pushed_at"),
            ),
            "branch" => format!("  {}", short(item, "name")),
            "commit" => format!(
                "  {}  {}  ({}, {})",
                short(item, "sha").chars().take(8).collect::<String>(),
                short(item, "commit.message").replace('\n', " "),
                item.pointer("/commit/author/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-"),
                item.pointer("/commit/author/date")
                    .and_then(|v| v.as_str())
                    .map(|d| d.chars().take(10).collect::<String>())
                    .unwrap_or_else(|| "-".into()),
            ),
            "pull request" => format!(
                "  #{}  {}  [{}]  {} -> {}  ({})",
                item.get("number").and_then(|v| v.as_i64()).unwrap_or(0),
                short(item, "title"),
                short(item, "state"),
                short(item, "head").if_empty("-"),
                short(item, "base").if_empty("-"),
                day(item, "updated_at"),
            ),
            "issue" => format!(
                "  #{}  {}  [{}]  ({} labels)",
                item.get("number").and_then(|v| v.as_i64()).unwrap_or(0),
                short(item, "title"),
                short(item, "state"),
                item.get("labels")
                    .and_then(|v| v.as_array())
                    .map(|l| l.len())
                    .unwrap_or(0),
            ),
            "release" => format!(
                "  {}  {}  ({})",
                short(item, "tag_name"),
                short(item, "name"),
                day(item, "published_at"),
            ),
            "workflow run" => format!(
                "  {}  {}  [{}]  {}  ({})",
                short(item, "name"),
                short(item, "head_branch"),
                short(item, "conclusion").if_empty(&short(item, "status")),
                short(item, "head_sha").chars().take(8).collect::<String>(),
                day(item, "updated_at"),
            ),
            _ => format!("  {item}"),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

trait IfEmpty {
    fn if_empty(&self, fallback: &str) -> String;
}
impl IfEmpty for String {
    fn if_empty(&self, fallback: &str) -> String {
        if self.is_empty() || self == "-" {
            fallback.to_string()
        } else {
            self.clone()
        }
    }
}

/// Nested pull_request fields (`head.ref`, `base.ref`) flattened for the list view.
fn flatten_pr(item: &Value) -> Value {
    let mut copy = item.clone();
    if let Some(object) = copy.as_object_mut() {
        for side in ["head", "base"] {
            let reference = item
                .pointer(&format!("/{side}/ref"))
                .and_then(|v| v.as_str())
                .unwrap_or("-")
                .to_string();
            object.insert(side.to_string(), Value::String(reference));
        }
    }
    copy
}

fn per_page(args: &Value) -> u32 {
    args.get("per_page")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .clamp(1, 50) as u32
}

fn need_repo(state: &AppState, args: &Value) -> Result<String, String> {
    if let Some(repo) = args.get("repo").and_then(|v| v.as_str()) {
        if !repo.trim().is_empty() {
            return Ok(repo.trim().to_string());
        }
    }
    repo_from_workspace(state).ok_or_else(|| {
        "no repository: pass repo=\"owner/name\" (no git remote with a GitHub URL in \
         the current workspace)"
            .to_string()
    })
}

fn need(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing required argument '{key}' for this action"))
}

#[async_trait]
impl crate::tools::AsyncTool for GithubTool {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "github",
            description: "Work with GitHub repositories directly: view/create repos, \
                branches, commits, pull requests, issues, releases and workflow runs. \
                Uses a native REST client (no `gh` process per call). Reads are cached \
                for a few seconds. Destructive or public-facing writes ask for approval.",
            emoji: "🐙",
            toolset: "github",
            is_async: true,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &GITHUB_SCHEMA
    }

    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let Some(kind) = action_kind(&action) else {
            return tool_error(format!(
                "unknown action '{action}'. Reads: auth, repo_view, repo_list, branch_list, \
                 commit_list, pr_list, pr_view, issue_list, release_list, run_list, \
                 search_repos, api. Writes: repo_create, repo_delete, pr_create, pr_merge, \
                 issue_create, issue_close, release_create."
            ));
        };

        let (token, source) = match resolve_token(&state, state.config.github.use_gh_cli) {
            Ok(v) => v,
            Err(e) => return tool_error(&e),
        };

        // Gate the writes that leave a trace on GitHub.
        if needs_approval(&action, &args) {
            let detail = format!(
                "github {action} {}",
                args.get("repo").and_then(|v| v.as_str()).unwrap_or("")
            );
            if let Err(message) = crate::guard::approve_async(
                &state,
                "github",
                detail.trim(),
                approval_reason(&action),
            )
            .await
            {
                return tool_error(&message);
            }
        }
        if kind == Kind::Write {
            invalidate_read_cache();
        }

        let method = reqwest::Method::from_bytes(
            args.get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET")
                .to_uppercase()
                .as_bytes(),
        )
        .unwrap_or(reqwest::Method::GET);

        // Build (path, body) per action.
        let (method, path, body): (reqwest::Method, String, Option<Value>) = match action.as_str() {
            "auth" => (reqwest::Method::GET, "/user".to_string(), None),
            "repo_view" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                (reqwest::Method::GET, format!("/repos/{repo}"), None)
            }
            "repo_list" => {
                let limit = per_page(&args);
                match args.get("org").and_then(|v| v.as_str()) {
                    Some(org) if !org.trim().is_empty() => (
                        reqwest::Method::GET,
                        format!("/orgs/{}/repos?per_page={limit}&sort=pushed", org.trim()),
                        None,
                    ),
                    _ => (
                        reqwest::Method::GET,
                        format!("/user/repos?per_page={limit}&sort=pushed&affiliation=owner,collaborator"),
                        None,
                    ),
                }
            }
            "repo_create" => {
                let name = match need(&args, "name") {
                    Ok(n) => n,
                    Err(e) => return tool_error(&e),
                };
                let private = args
                    .get("private")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                (
                    reqwest::Method::POST,
                    "/user/repos".to_string(),
                    Some(json!({
                        "name": name,
                        "private": private,
                        "description": args.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                        "auto_init": true,
                    })),
                )
            }
            "repo_delete" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                (reqwest::Method::DELETE, format!("/repos/{repo}"), None)
            }
            "branch_list" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                (
                    reqwest::Method::GET,
                    format!("/repos/{repo}/branches?per_page={}", per_page(&args)),
                    None,
                )
            }
            "commit_list" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                (
                    reqwest::Method::GET,
                    format!("/repos/{repo}/commits?per_page={}", per_page(&args)),
                    None,
                )
            }
            "pr_list" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                let state_filter = args.get("state").and_then(|v| v.as_str()).unwrap_or("open");
                (
                    reqwest::Method::GET,
                    format!(
                        "/repos/{repo}/pulls?state={state_filter}&per_page={}",
                        per_page(&args)
                    ),
                    None,
                )
            }
            "pr_view" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                let number = match args.get("number").and_then(|v| v.as_i64()) {
                    Some(n) => n,
                    None => return tool_error("pr_view needs 'number'"),
                };
                (
                    reqwest::Method::GET,
                    format!("/repos/{repo}/pulls/{number}"),
                    None,
                )
            }
            "pr_create" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                let (title, head) = match (need(&args, "title"), need(&args, "head")) {
                    (Ok(t), Ok(h)) => (t, h),
                    (Err(e), _) | (_, Err(e)) => return tool_error(&e),
                };
                let base = args
                    .get("base")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let mut payload = json!({
                    "title": title,
                    "head": head,
                    "body": args.get("body").and_then(|v| v.as_str()).unwrap_or(""),
                });
                if let Some(base) = base {
                    payload["base"] = Value::String(base);
                }
                (
                    reqwest::Method::POST,
                    format!("/repos/{repo}/pulls"),
                    Some(payload),
                )
            }
            "pr_merge" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                let number = match args.get("number").and_then(|v| v.as_i64()) {
                    Some(n) => n,
                    None => return tool_error("pr_merge needs 'number'"),
                };
                (
                    reqwest::Method::PUT,
                    format!("/repos/{repo}/pulls/{number}/merge"),
                    Some(json!({ "merge_method": "squash" })),
                )
            }
            "issue_list" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                let state_filter = args.get("state").and_then(|v| v.as_str()).unwrap_or("open");
                (
                    reqwest::Method::GET,
                    format!(
                        "/repos/{repo}/issues?state={state_filter}&per_page={}",
                        per_page(&args)
                    ),
                    None,
                )
            }
            "issue_create" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                let title = match need(&args, "title") {
                    Ok(t) => t,
                    Err(e) => return tool_error(&e),
                };
                (
                    reqwest::Method::POST,
                    format!("/repos/{repo}/issues"),
                    Some(json!({
                        "title": title,
                        "body": args.get("body").and_then(|v| v.as_str()).unwrap_or(""),
                    })),
                )
            }
            "issue_close" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                let number = match args.get("number").and_then(|v| v.as_i64()) {
                    Some(n) => n,
                    None => return tool_error("issue_close needs 'number'"),
                };
                (
                    reqwest::Method::PATCH,
                    format!("/repos/{repo}/issues/{number}"),
                    Some(json!({ "state": "closed" })),
                )
            }
            "release_list" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                (
                    reqwest::Method::GET,
                    format!("/repos/{repo}/releases?per_page={}", per_page(&args)),
                    None,
                )
            }
            "release_create" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                let tag = match need(&args, "tag") {
                    Ok(t) => t,
                    Err(e) => return tool_error(&e),
                };
                (
                    reqwest::Method::POST,
                    format!("/repos/{repo}/releases"),
                    Some(json!({
                        "tag_name": tag,
                        "name": args.get("title").and_then(|v| v.as_str()).unwrap_or(&tag),
                        "body": args.get("body").and_then(|v| v.as_str()).unwrap_or(""),
                    })),
                )
            }
            "run_list" => {
                let repo = match need_repo(&state, &args) {
                    Ok(r) => r,
                    Err(e) => return tool_error(&e),
                };
                (
                    reqwest::Method::GET,
                    format!("/repos/{repo}/actions/runs?per_page={}", per_page(&args)),
                    None,
                )
            }
            "search_repos" => {
                let query = match need(&args, "query") {
                    Ok(q) => q,
                    Err(e) => return tool_error(&e),
                };
                let encoded: String = query
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || "-_.~/".contains(c) {
                            c.to_string()
                        } else {
                            format!("%{:02X}", c as u32)
                        }
                    })
                    .collect();
                (
                    reqwest::Method::GET,
                    format!(
                        "/search/repositories?q={encoded}&per_page={}",
                        per_page(&args)
                    ),
                    None,
                )
            }
            "api" => {
                let path = match need(&args, "query") {
                    Ok(p) => p,
                    Err(_e) => {
                        return tool_error(
                            "api needs 'query' (or 'path') holding the API path, e.g. /repos/o/r/labels",
                        )
                    }
                };
                let path = if path.starts_with('/') {
                    path
                } else {
                    format!("/{path}")
                };
                (method, path, None)
            }
            _ => unreachable!("action_kind validated the name"),
        };

        // Cache only safe reads. The key holds the full URL: two API bases (a
        // stub in tests, the real one in production) must never share entries.
        let url = if path.starts_with("http") {
            path.clone()
        } else {
            format!("{}{}", api_base(&state), path)
        };
        let cache_key = format!("{} {}", method, url);
        let raw = args.get("raw").and_then(|v| v.as_bool()).unwrap_or(false);
        if method == reqwest::Method::GET && !raw {
            if let Some((body, age)) = cached_get(&cache_key, cache_ttl(&state)) {
                return format!("{body}(cached, {age}s old)\n");
            }
        }

        let response = match request(&token, method.clone(), &url, body).await {
            Ok(r) => r,
            Err(e) => return tool_error(&e),
        };

        if !response.status.is_success() {
            return tool_error(format!(
                "{}{}",
                error_message(response.status, &response.body),
                rate_hint()
            ));
        }

        if raw {
            let pretty = serde_json::to_string_pretty(&response.body).unwrap_or_default();
            let cut = pretty.chars().take(RAW_LIMIT).collect::<String>();
            let suffix = if pretty.chars().count() > RAW_LIMIT {
                format!("\n… truncated at {RAW_LIMIT} characters\n")
            } else {
                String::new()
            };
            return format!("{cut}{suffix}");
        }

        let text = match action.as_str() {
            "auth" => {
                let body = &response.body;
                let out = format!(
                    "authenticated as {} ({}), token from {}\n",
                    short(body, "login"),
                    short(body, "type"),
                    source
                );
                let hint = rate_hint();
                let out = if hint.is_empty() {
                    out
                } else {
                    format!("{out}{hint}\n")
                };
                out
            }
            "repo_view" => summarize_repo(&response.body),
            "repo_list" | "branch_list" | "commit_list" | "issue_list" | "release_list" => {
                let kind = match action.as_str() {
                    "repo_list" => "repository",
                    "branch_list" => "branch",
                    "commit_list" => "commit",
                    "issue_list" => "issue",
                    _ => "release",
                };
                let items: Vec<Value> = response
                    .body
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|item| item.get("pull_request").is_none())
                    .collect();
                summarize_list(&items, kind)
            }
            "pr_list" => {
                let items: Vec<Value> = response
                    .body
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(flatten_pr)
                    .collect();
                summarize_list(&items, "pull request")
            }
            "pr_view" => {
                let body = &response.body;
                format!(
                    "PR #{} [{}] {}\n  {} -> {}   mergeable: {}\n  author: {}   updated: {}\n  url: {}\n  body:\n{}\n",
                    body.get("number").and_then(|v| v.as_i64()).unwrap_or(0),
                    short(body, "state"),
                    short(body, "title"),
                    body.pointer("/head/ref").and_then(|v| v.as_str()).unwrap_or("-"),
                    body.pointer("/base/ref").and_then(|v| v.as_str()).unwrap_or("-"),
                    body.get("mergeable").map(|v| v.to_string()).unwrap_or_else(|| "unknown".into()),
                    body.pointer("/user/login").and_then(|v| v.as_str()).unwrap_or("-"),
                    day(body, "updated_at"),
                    short(body, "html_url"),
                    body.get("body").and_then(|v| v.as_str()).unwrap_or("").chars().take(1500).collect::<String>(),
                )
            }
            "run_list" => {
                let items: Vec<Value> = response
                    .body
                    .pointer("/workflow_runs")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                summarize_list(&items, "workflow run")
            }
            "search_repos" => {
                let total = response
                    .body
                    .get("total_count")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let items: Vec<Value> = response
                    .body
                    .pointer("/items")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                format!(
                    "{total} repositories matched\n{}",
                    summarize_list(&items, "repository")
                )
            }
            "repo_create" => format!("created:\n{}", summarize_repo(&response.body)),
            "repo_delete" => format!(
                "deleted {} (HTTP {})\n",
                args.get("repo").and_then(|v| v.as_str()).unwrap_or("-"),
                response.status.as_u16()
            ),
            "pr_create" => {
                let body = &response.body;
                format!(
                    "opened PR #{} {} -> {}\n  {}\n  {}\n",
                    body.get("number").and_then(|v| v.as_i64()).unwrap_or(0),
                    body.pointer("/head/ref")
                        .and_then(|v| v.as_str())
                        .unwrap_or("-"),
                    body.pointer("/base/ref")
                        .and_then(|v| v.as_str())
                        .unwrap_or("-"),
                    short(body, "title"),
                    short(body, "html_url"),
                )
            }
            "pr_merge" => format!(
                "merge result: {} (sha {})\n",
                short(&response.body, "message"),
                short(&response.body, "sha")
            ),
            "issue_create" | "issue_close" => {
                let body = &response.body;
                format!(
                    "issue #{} [{}] {}\n  {}\n",
                    body.get("number").and_then(|v| v.as_i64()).unwrap_or(0),
                    short(body, "state"),
                    short(body, "title"),
                    short(body, "html_url"),
                )
            }
            "release_create" => {
                let body = &response.body;
                format!(
                    "release {} ({}) published: {}\n",
                    short(body, "tag_name"),
                    short(body, "name"),
                    short(body, "html_url")
                )
            }
            _ => serde_json::to_string_pretty(&response.body).unwrap_or_default(),
        };

        if method == reqwest::Method::GET {
            let ttl = cache_ttl(&state);
            if ttl > 0 {
                READ_CACHE
                    .lock()
                    .unwrap()
                    .insert(cache_key, (Instant::now(), text.clone()));
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AsyncTool;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    lazy_static::lazy_static! {
        /// Tests that mutate process-wide environment (token, approval flag) have
        /// to serialize: one test removing GITHUB_TOKEN while another sets it
        /// makes both results meaningless.
        static ref ENV_LOCK: Mutex<()> = Mutex::new(());
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Minimal HTTP/1.1 stub: counts requests, answers every one with the same
    /// canned JSON. Enough to prove request shape, summarising and caching
    /// without talking to GitHub.
    struct MockGithub {
        port: u16,
        hits: Arc<Mutex<usize>>,
        last_path: Arc<Mutex<String>>,
    }

    fn mock(status_line: &'static str, body: &'static str) -> MockGithub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(Mutex::new(0usize));
        let last_path = Arc::new(Mutex::new(String::new()));
        let hits_thread = hits.clone();
        let path_thread = last_path.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buffer = [0u8; 4096];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let first_line = request.lines().next().unwrap_or("").to_string();
                *path_thread.lock().unwrap() = first_line.clone();
                *hits_thread.lock().unwrap() += 1;
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-ratelimit-remaining: 4999\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        MockGithub {
            port,
            hits,
            last_path,
        }
    }

    fn state_for(api_base: &str) -> Arc<AppState> {
        let mut config = crate::config::Config::default();
        config.github.api_base = Some(api_base.to_string());
        config.github.cache_ttl_secs = 30;
        config.data_dir = std::env::temp_dir().join("rijo_github_test");
        std::fs::create_dir_all(&config.data_dir).unwrap();
        Arc::new(AppState {
            brain: Arc::new(
                crate::brain::db::BrainDb::open(&config.data_dir.join("brain.db"), 384).unwrap(),
            ),
            embedder: tokio::sync::Mutex::new(None),
            config,
            session_id: crate::AppState::new_session_id(),
            tools: Arc::new(crate::tools::Registry::build_default()),
            ui_tx: std::sync::Mutex::new(None),
            files_read: std::sync::Mutex::new(std::collections::HashSet::new()),
            cancel: std::sync::atomic::AtomicBool::new(false),
            router: std::sync::Mutex::new(None),
        })
    }

    fn repo_json() -> &'static str {
        r#"{"full_name":"rijoda/rijo-code","private":true,"default_branch":"master","html_url":"https://github.com/rijoda/rijo-code","clone_url":"https://github.com/rijoda/rijo-code.git","pushed_at":"2026-09-16T21:00:00Z","created_at":"2026-09-15T10:00:00Z","size":650,"open_issues_count":2,"stargazers_count":0,"description":"Rust agent harness","permissions":{"push":true,"admin":true}}"#
    }

    #[test]
    fn parses_every_github_remote_form() {
        assert_eq!(
            parse_remote("git@github.com:rijoda/rijo-code.git").as_deref(),
            Some("rijoda/rijo-code")
        );
        assert_eq!(
            parse_remote("https://github.com/rijoda/rijo-code.git").as_deref(),
            Some("rijoda/rijo-code")
        );
        assert_eq!(
            parse_remote("https://github.com/rijoda/rijo-code").as_deref(),
            Some("rijoda/rijo-code")
        );
        assert_eq!(
            parse_remote("ssh://git@github.com/rijoda/rijo-code.git").as_deref(),
            Some("rijoda/rijo-code")
        );
        // not GitHub, or not a repo URL at all
        assert_eq!(parse_remote("git@gitlab.com:someone/thing.git"), None);
        assert_eq!(parse_remote("/home/user/local"), None);
        assert_eq!(parse_remote(""), None);
    }

    #[test]
    fn writes_and_reads_are_classified() {
        for read in ["auth", "repo_view", "pr_list", "run_list", "api"] {
            assert!(action_is_read(read), "{read}");
            assert!(
                !needs_approval(read, &json!({})),
                "{read} must not need approval"
            );
        }
        for write in ["pr_merge", "repo_delete", "release_create", "issue_create"] {
            assert_eq!(action_kind(write), Some(Kind::Write), "{write}");
            assert!(!action_is_read(write), "{write} must not batch");
        }
        // Approval covers the irreversible and the public-facing writes only.
        assert!(needs_approval("pr_merge", &json!({})));
        assert!(needs_approval("repo_delete", &json!({})));
        assert!(needs_approval("release_create", &json!({})));
        assert!(!needs_approval("issue_create", &json!({})));
        assert!(!needs_approval("issue_close", &json!({})));
        // A PRIVATE repo keeps its default (no approval), a PUBLIC one asks.
        assert!(!needs_approval("repo_create", &json!({"private": true})));
        assert!(!needs_approval("repo_create", &json!({})));
        assert!(needs_approval("repo_create", &json!({"private": false})));
        // Unknown actions are neither readable nor approvable.
        assert_eq!(action_kind("nonsense"), None);
        assert!(!action_is_read("nonsense"));
        assert!(!needs_approval("nonsense", &json!({})));
    }

    #[tokio::test]
    async fn repo_view_is_summarised_and_then_served_from_cache() {
        let _guard = env_lock();
        let server = mock("200 OK", repo_json());
        let state = state_for(&format!("http://127.0.0.1:{}", server.port));
        std::env::set_var("GITHUB_TOKEN", "test-token-not-real");
        let tool = GithubTool;

        let first = tool
            .call_async(
                json!({"action": "repo_view", "repo": "rijoda/rijo-code"}),
                state.clone(),
            )
            .await;
        assert!(first.contains("rijoda/rijo-code"), "{first}");
        assert!(first.contains("default:     master"), "{first}");
        assert!(first.contains("private"), "{first}");
        assert_eq!(*server.hits.lock().unwrap(), 1);

        // Second call inside the TTL: no new HTTP request, marked as cached.
        let second = tool
            .call_async(
                json!({"action": "repo_view", "repo": "rijoda/rijo-code"}),
                state.clone(),
            )
            .await;
        assert_eq!(*server.hits.lock().unwrap(), 1, "cache miss: {second}");
        assert!(second.contains("cached"), "{second}");

        // raw=true bypasses the cache on purpose.
        let third = tool
            .call_async(
                json!({"action": "repo_view", "repo": "rijoda/rijo-code", "raw": true}),
                state.clone(),
            )
            .await;
        assert_eq!(*server.hits.lock().unwrap(), 2, "{third}");
        assert!(third.contains("\"full_name\""), "{third}");
    }

    #[tokio::test]
    async fn a_write_invalidates_the_read_cache() {
        let _guard = env_lock();
        let server = mock("200 OK", repo_json());
        let state = state_for(&format!("http://127.0.0.1:{}", server.port));
        std::env::set_var("GITHUB_TOKEN", "test-token-not-real");
        // No RIJO_APPROVE_DANGEROUS here: filing an issue is reversible and must
        // run without a dialog (that is the behaviour under test).
        let tool = GithubTool;

        let _ = tool
            .call_async(
                json!({"action": "repo_view", "repo": "rijoda/rijo-code"}),
                state.clone(),
            )
            .await;
        assert_eq!(*server.hits.lock().unwrap(), 1);

        // issue_create is a write: it must drop the cached read.
        let _ = tool
            .call_async(
                json!({"action": "issue_create", "repo": "rijoda/rijo-code", "title": "x"}),
                state.clone(),
            )
            .await;
        let _ = tool
            .call_async(
                json!({"action": "repo_view", "repo": "rijoda/rijo-code"}),
                state.clone(),
            )
            .await;
        assert_eq!(
            *server.hits.lock().unwrap(),
            3,
            "the cached repo_view survived a write"
        );
        std::env::remove_var("RIJO_APPROVE_DANGEROUS");
    }

    #[tokio::test]
    async fn errors_explain_what_to_do() {
        let _guard = env_lock();
        let server = mock("404 Not Found", r#"{"message":"Not Found"}"#);
        let state = state_for(&format!("http://127.0.0.1:{}", server.port));
        std::env::set_var("GITHUB_TOKEN", "test-token-not-real");
        let out = GithubTool
            .call_async(
                json!({"action": "repo_view", "repo": "nobody/nothing"}),
                state.clone(),
            )
            .await;
        assert!(out.contains("404"), "{out}");
        assert!(out.contains("not found"), "{out}");

        let server = mock("401 Unauthorized", r#"{"message":"Bad credentials"}"#);
        let state_401 = state_for(&format!("http://127.0.0.1:{}", server.port));
        let out = GithubTool
            .call_async(json!({"action": "auth"}), state_401.clone())
            .await;
        assert!(out.contains("Bad credentials"), "{out}");
        assert!(
            out.contains("gh auth login") || out.contains("GITHUB_TOKEN"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn a_missing_token_is_reported_with_the_options() {
        let _guard = env_lock();
        // Both env vars and the gh CLI are out of the picture for this call.
        std::env::remove_var("GITHUB_TOKEN");
        std::env::remove_var("GH_TOKEN");
        let mut config = crate::config::Config::default();
        config.github.api_base = Some("http://127.0.0.1:1".into());
        config.github.use_gh_cli = false;
        let state = Arc::new(AppState {
            brain: Arc::new(
                crate::brain::db::BrainDb::open(&config.data_dir.join("brain.db"), 384).unwrap(),
            ),
            embedder: tokio::sync::Mutex::new(None),
            config,
            session_id: crate::AppState::new_session_id(),
            tools: Arc::new(crate::tools::Registry::build_default()),
            ui_tx: std::sync::Mutex::new(None),
            files_read: std::sync::Mutex::new(std::collections::HashSet::new()),
            cancel: std::sync::atomic::AtomicBool::new(false),
            router: std::sync::Mutex::new(None),
        });
        let out = GithubTool
            .call_async(json!({"action": "auth"}), state)
            .await;
        assert!(out.contains("no GitHub token found"), "{out}");
        assert!(out.contains("token_env"), "{out}");
    }

    #[tokio::test]
    async fn list_actions_ask_for_the_right_endpoint() {
        let _guard = env_lock();
        let server = mock(
            "200 OK",
            r#"[{"number":7,"title":"fix cache","state":"open","updated_at":"2026-09-16T10:00:00Z","head":{"ref":"fix/cache"},"base":{"ref":"master"}}]"#,
        );
        let state = state_for(&format!("http://127.0.0.1:{}", server.port));
        std::env::set_var("GITHUB_TOKEN", "test-token-not-real");
        let out = GithubTool
            .call_async(
                json!({"action": "pr_list", "repo": "rijoda/rijo-code"}),
                state.clone(),
            )
            .await;
        assert!(out.contains("#7"), "{out}");
        assert!(out.contains("fix/cache -> master"), "{out}");
        let seen = server.last_path.lock().unwrap().clone();
        assert!(
            seen.contains("/repos/rijoda/rijo-code/pulls?state=open"),
            "{seen}"
        );
    }
}
