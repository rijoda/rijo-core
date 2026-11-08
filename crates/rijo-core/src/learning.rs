//! Learning review after the turn — Hermes parity.
//!
//! Why not the old way: `skills.rs` only counted tool names and wrote
//! template files from them to `<skills_dir>/_auto/tool-<name>/SKILL.md`
//! ("The agent used `terminal` repeatedly"). That was not knowledge, was not
//! even listed by `skills_list` (one level too deep) and improved
//! nothing. See docs/GOAL-100.md section Q.
//!
//! What happens here (like Hermes' `background_review.py`): after a turn
//! a cheap LLM call receives a compressed turn excerpt and
//! exactly two questions — what was not obvious, and does that belong in a
//! skill or in memory? The answer is forced into a machine-readable
//! format (SKILL_CREATE / SKILL_UPDATE / MEMORY), checked and
//! applied. Writing happens ONLY to `skills_dir` and `data_dir/memories`
//! — never into a foreign skill library.
//!
//! Cost: one call per reviewed turn, capped via
//! `learning.max_tokens`; every decision ends up as its own line
//! (`kind: "learn"`) in `turn_metrics.jsonl`.

use crate::config::LearningConfig;
use crate::llm::{ChatMessage, LlmClient};
use std::path::{Path, PathBuf};

/// One tool call within a turn.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCallRecord {
    pub tool: String,
    pub args_summary: String,
    pub success: bool,
    pub duration_ms: u64,
}

/// Compressed excerpt of a turn — the input for the review.
#[derive(Debug, Clone)]
pub struct TurnDigest {
    pub user_message: String,
    pub answer: String,
    pub iterations: u32,
    pub tool_calls: Vec<ToolCallRecord>,
}

impl TurnDigest {
    pub fn new(
        user_message: &str,
        answer: &str,
        iterations: u32,
        tool_calls: Vec<ToolCallRecord>,
    ) -> Self {
        Self {
            user_message: user_message.to_string(),
            answer: answer.to_string(),
            iterations,
            tool_calls,
        }
    }

    /// Is the review worth it? Trivial turns ("answer with ok") otherwise cost
    /// an LLM call every turn, without anything to learn.
    pub fn worth_reviewing(&self, cfg: &LearningConfig) -> bool {
        self.iterations >= cfg.min_iterations && !self.tool_calls.is_empty()
    }

    /// Were there failed attempts or repeats? Such turns are the
    /// most productive ones for lessons.
    pub fn has_friction(&self) -> bool {
        let failures = self.tool_calls.iter().filter(|c| !c.success).count();
        let mut seen: Vec<(&str, &str)> = Vec::new();
        let mut repeats = 0;
        for c in &self.tool_calls {
            let key = (c.tool.as_str(), c.args_summary.as_str());
            if seen.contains(&key) {
                repeats += 1;
            }
            seen.push(key);
        }
        failures > 0 || repeats > 0
    }
}

const REVIEW_SYSTEM: &str = "\
You review one finished agent turn and decide whether anything durable should be \
learned. You have no tools: answer in the exact format below, nothing else.

Rules:
- A skill is a reusable procedure with concrete commands, pitfalls or conventions. \
  \"I used terminal three times\" is NOT a skill.
- Only write something that future turns will profit from. If the turn was \
  routine (a normal edit, a normal build), answer NOTHING.
- Prefer updating an existing skill over creating a near-duplicate.
- Never invent commands, flags or file paths you did not see in the turn.
- Memory entries are single declarative facts, no instructions.
- Write skills and memory entries in ENGLISH, whatever language the turn was in.
  They are long-lived artifacts other sessions read, not a reply to the user.
- Maximum 3 blocks. Fewer is better. Empty answer is a valid, common outcome.";

/// Builds the review prompt (pure function — testable without a model).
pub fn build_review_prompt(d: &TurnDigest, existing_skills: &[String]) -> String {
    let mut calls = String::new();
    for (i, c) in d.tool_calls.iter().enumerate().take(24) {
        calls.push_str(&format!(
            "{}. {} {} ({} ms, {}) args={}\n",
            i + 1,
            if c.success { "OK  " } else { "FAIL" },
            c.tool,
            c.duration_ms,
            if c.success { "ok" } else { "error" },
            c.args_summary.chars().take(120).collect::<String>()
        ));
    }
    let skills = if existing_skills.is_empty() {
        "(none yet)".to_string()
    } else {
        existing_skills.join(", ")
    };

    format!(
        "{REVIEW_SYSTEM}\n\n\
         Existing skills you may extend: {skills}\n\n\
         --- USER REQUEST ---\n{user}\n\n\
         --- TOOL CALLS ({calls_n} total, {iters} loop iterations) ---\n{list}\n\
         --- FINAL ANSWER (excerpt) ---\n{answer}\n\
         --- END ---\n\n\
         Answer format (one block per line start, no markdown fences):\n\
         SKILL_CREATE: <lowercase-hyphenated-name>\n\
         description: <one sentence, max 60 chars>\n\
         <markdown body: when to use it, the exact commands, the pitfalls>\n\n\
         SKILL_UPDATE: <existing-skill-name>\n\
         <the lesson to append, with the date and the concrete command>\n\n\
         MEMORY: <one declarative fact worth remembering across sessions>\n\n\
         If nothing is worth learning: NOTHING",
        calls_n = d.tool_calls.len(),
        list = calls,
        user = d.user_message.chars().take(1000).collect::<String>(),
        iters = d.iterations,
        answer = d.answer.chars().take(1500).collect::<String>(),
    )
}

/// A parsed learning action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    CreateSkill {
        name: String,
        description: String,
        body: String,
    },
    UpdateSkill {
        name: String,
        text: String,
    },
    Memory {
        text: String,
    },
}

/// Only harmless names: no path separators, no `..`, no special characters.
pub fn safe_skill_name(raw: &str) -> Option<String> {
    let name = raw.trim().trim_matches('"').to_ascii_lowercase();
    if name.is_empty() || name.len() > 64 {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return None;
    }
    if name.starts_with('-') || name.starts_with('_') || name.contains("..") {
        return None;
    }
    Some(name)
}

/// Splits the model answer into actions. Anything unknown is ignored.
pub fn parse_actions(text: &str, max: usize) -> Vec<Action> {
    if text.trim().eq_ignore_ascii_case("NOTHING") {
        return Vec::new();
    }
    let mut actions: Vec<Action> = Vec::new();
    let mut mode: Option<String> = None; // "create" | "update" | "memory"
    let mut name = String::new();
    let mut description = String::new();
    let mut body = String::new();
    let mut pending_desc = false;

    fn flush(
        actions: &mut Vec<Action>,
        mode: &Option<String>,
        name: &str,
        description: &str,
        body: &str,
    ) {
        let text = body.trim().to_string();
        match mode.as_deref() {
            Some("create") => {
                if let Some(n) = safe_skill_name(name) {
                    let desc: String = description.trim().chars().take(70).collect();
                    if !text.is_empty() && !desc.is_empty() {
                        actions.push(Action::CreateSkill {
                            name: n,
                            description: desc,
                            body: text,
                        });
                    }
                }
            }
            Some("update") => {
                if let Some(n) = safe_skill_name(name) {
                    if !text.is_empty() {
                        actions.push(Action::UpdateSkill { name: n, text });
                    }
                }
            }
            Some("memory") => {
                let line = text.lines().next().unwrap_or("").trim().to_string();
                if !line.is_empty() {
                    actions.push(Action::Memory { text: line });
                }
            }
            _ => {}
        }
    }

    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        let upper = line.trim_start().to_ascii_uppercase();
        if upper.starts_with("SKILL_CREATE:") {
            flush(&mut actions, &mode, &name, &description, &body);
            mode = Some("create".into());
            name = line.trim_start()[13..].trim().to_string();
            description.clear();
            body.clear();
            pending_desc = true;
            continue;
        }
        if upper.starts_with("SKILL_UPDATE:") {
            flush(&mut actions, &mode, &name, &description, &body);
            mode = Some("update".into());
            name = line.trim_start()[13..].trim().to_string();
            description.clear();
            body.clear();
            pending_desc = false;
            continue;
        }
        if upper.starts_with("MEMORY:") {
            flush(&mut actions, &mode, &name, &description, &body);
            mode = Some("memory".into());
            name.clear();
            description.clear();
            body = line.trim_start()[7..].trim().to_string();
            pending_desc = false;
            continue;
        }
        if pending_desc && upper.starts_with("DESCRIPTION:") {
            description = line.trim_start()[12..].trim().to_string();
            pending_desc = false;
            continue;
        }
        if mode.is_some() && !line.starts_with("```") {
            body.push_str(line);
            body.push('\n');
        }
    }
    flush(&mut actions, &mode, &name, &description, &body);
    actions.truncate(max);
    actions
}

/// Result of an applied action (for log + display).
pub struct Applied {
    pub kind: &'static str,
    pub target: String,
    pub detail: String,
}

fn skill_md(name: &str, description: &str, body: &str, source: &str) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    format!(
        "---\nname: {name}\ndescription: \"{description}\"\nversion: 0.1.0\nmetadata:\n  rijo:\n    source: {source}\n    created: {now}\n---\n\n{body}\n"
    )
}

/// Applies parsed actions. Writes exclusively to `skills_dir`
/// (skills) and `<data_dir>/memories/MEMORY.md` (facts).
pub fn apply_actions(
    actions: &[Action],
    skills_dir: &Path,
    data_dir: &Path,
    max_memory_chars: usize,
) -> Vec<Applied> {
    let mut done = Vec::new();
    let memory_file = data_dir.join("memories").join("MEMORY.md");
    const DELIM: &str = "\n§\n";

    for action in actions {
        match action {
            Action::CreateSkill {
                name,
                description,
                body,
            } => {
                let dir = skills_dir.join(name);
                let path = dir.join("SKILL.md");
                let body: String = body.chars().take(4000).collect();
                if path.exists() {
                    // No overwriting: append as a lesson instead of replacing it.
                    if append_lesson(&path, &body) {
                        done.push(Applied {
                            kind: "skill-update",
                            target: name.clone(),
                            detail: "already existed — lesson appended".into(),
                        });
                    }
                } else if std::fs::create_dir_all(&dir).is_ok()
                    && std::fs::write(&path, skill_md(name, description, &body, "turn-review"))
                        .is_ok()
                {
                    done.push(Applied {
                        kind: "skill-create",
                        target: name.clone(),
                        detail: description.clone(),
                    });
                }
            }
            Action::UpdateSkill { name, text } => {
                let path = skills_dir.join(name).join("SKILL.md");
                if path.exists() {
                    let text: String = text.chars().take(2000).collect();
                    if append_lesson(&path, &text) {
                        done.push(Applied {
                            kind: "skill-update",
                            target: name.clone(),
                            detail: "lesson appended".into(),
                        });
                    }
                } else {
                    // Skill lives in a FOREIGN library or does not exist:
                    // don't touch it, just note it.
                    let foreign = crate::config::Config::load(None)
                        .skill_roots
                        .iter()
                        .any(|root| root.join(name).join("SKILL.md").exists());
                    done.push(Applied {
                        kind: "skipped",
                        target: name.clone(),
                        detail: if foreign {
                            "Skill lives in a foreign library — not written".into()
                        } else {
                            "Skill does not exist".into()
                        },
                    });
                }
            }
            Action::Memory { text } => {
                if let Some(parent) = memory_file.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let existing = std::fs::read_to_string(&memory_file).unwrap_or_default();
                let mut entries: Vec<String> = existing
                    .split(DELIM)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if entries.iter().any(|e| e.eq_ignore_ascii_case(text)) {
                    continue;
                }
                let candidate = entries
                    .iter()
                    .cloned()
                    .chain(std::iter::once(text.clone()))
                    .collect::<Vec<_>>()
                    .join(DELIM);
                if candidate.chars().count() <= max_memory_chars {
                    entries.push(text.clone());
                    if std::fs::write(&memory_file, entries.join(DELIM)).is_ok() {
                        done.push(Applied {
                            kind: "memory",
                            target: "MEMORY.md".into(),
                            detail: text.chars().take(80).collect(),
                        });
                    }
                } else {
                    done.push(Applied {
                        kind: "skipped",
                        target: "MEMORY.md".into(),
                        detail: "memory full — nothing written".into(),
                    });
                }
            }
        }
    }
    done
}

fn append_lesson(path: &Path, text: &str) -> bool {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return false;
    };
    let stamp = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let updated = format!(
        "{}\n\n## Learned {stamp}\n\n{}\n",
        existing.trim_end(),
        text.trim()
    );
    std::fs::write(path, updated).is_ok()
}

/// Collects the names of existing skills (for the prompt).
pub fn existing_skill_names(skills_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(skills_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.join("SKILL.md").exists() {
                if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                    names.push(n.to_string());
                }
            }
        }
    }
    names.sort();
    names.truncate(40);
    names
}

/// One review run: build the prompt, ask the model, apply the actions, log the
/// cost. Errors are swallowed — learning must never
/// disturb operation.
pub async fn review_turn(
    llm: &LlmClient,
    cfg: &LearningConfig,
    skills_dir: &Path,
    data_dir: &Path,
    digest: &TurnDigest,
) -> Vec<Applied> {
    let t0 = std::time::Instant::now();
    let existing = existing_skill_names(skills_dir);
    let prompt = build_review_prompt(digest, &existing);
    let messages = vec![
        ChatMessage {
            role: "system".into(),
            content: Some(REVIEW_SYSTEM.to_string()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            parts: None,
        },
        ChatMessage {
            role: "user".into(),
            content: Some(prompt),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            parts: None,
        },
    ];

    let (text, usage) = match llm.chat(&messages, &[]).await {
        Ok((text, _reasoning, _calls, usage)) => (text, usage),
        Err(e) => {
            log_metrics(
                data_dir,
                &digest.tool_calls.len().to_string(),
                None,
                t0.elapsed(),
                &[],
                &format!("error: {e}"),
            );
            return Vec::new();
        }
    };

    let actions = parse_actions(&text, cfg.max_actions);
    let applied = apply_actions(&actions, skills_dir, data_dir, 2200);
    log_metrics(
        data_dir,
        &digest.tool_calls.len().to_string(),
        usage.map(|u| (u.prompt, u.completion)),
        t0.elapsed(),
        &applied,
        "",
    );
    applied
}

/// Make the cost visible: one line per review in `turn_metrics.jsonl`.
fn log_metrics(
    data_dir: &Path,
    _calls: &str,
    usage: Option<(u32, u32)>,
    elapsed: std::time::Duration,
    applied: &[Applied],
    error: &str,
) {
    let path: PathBuf = data_dir.join("turn_metrics.jsonl");
    let (prompt_tokens, completion_tokens) = usage.unwrap_or((0, 0));
    let record = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "kind": "learn",
        "llm_ms": elapsed.as_millis() as u64,
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "applied": applied.iter().map(|a| format!("{}:{}", a.kind, a.target)).collect::<Vec<_>>(),
        "error": error,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
    {
        use std::io::Write;
        let _ = f.write_all(format!("{record}\n").as_bytes());
    }
}

/// A review client with a capped output budget (the main client may
/// answer 4096 tokens, the review needs <800).
pub fn review_client(cfg: &crate::config::LlmConfig, max_tokens: u32) -> LlmClient {
    let mut c = cfg.clone();
    c.max_tokens = max_tokens;
    c.temperature = 0.0;
    LlmClient::new(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> TurnDigest {
        TurnDigest::new(
            "build the page and put it live",
            "Server is running on 8795, curl 200.",
            4,
            vec![
                ToolCallRecord {
                    tool: "write_file".into(),
                    args_summary: "path=index.html".into(),
                    success: true,
                    duration_ms: 3,
                },
                ToolCallRecord {
                    tool: "terminal".into(),
                    args_summary: "python3 -m http.server 8795 &".into(),
                    success: false,
                    duration_ms: 51,
                },
                ToolCallRecord {
                    tool: "terminal".into(),
                    args_summary: "background=true http.server".into(),
                    success: true,
                    duration_ms: 2,
                },
            ],
        )
    }

    #[test]
    fn prompt_contains_turn_evidence() {
        let p = build_review_prompt(&digest(), &["rijo-rust-harness".to_string()]);
        assert!(
            p.contains("python3 -m http.server 8795 &"),
            "tool args missing"
        );
        assert!(p.contains("FAIL"), "failed attempts must be marked");
        assert!(p.contains("rijo-rust-harness"), "existing skills missing");
        assert!(p.contains("SKILL_CREATE:"), "format missing");
        assert!(
            p.contains("in ENGLISH"),
            "skills/memory must be written in English"
        );
    }

    #[test]
    fn trivial_turns_are_not_reviewed() {
        let cfg = LearningConfig::default();
        let quiet = TurnDigest::new("hi", "hello", 1, vec![]);
        assert!(
            !quiet.worth_reviewing(&cfg),
            "1 iteration without tools: no review"
        );
        let one_shot = TurnDigest::new(
            "do x",
            "ok",
            1,
            vec![ToolCallRecord {
                tool: "write_file".into(),
                args_summary: "a".into(),
                success: true,
                duration_ms: 1,
            }],
        );
        assert!(
            !one_shot.worth_reviewing(&cfg),
            "one pass without friction: no review"
        );
        assert!(
            digest().worth_reviewing(&cfg),
            "4 iterations with tools: review"
        );
        assert!(
            digest().has_friction(),
            "a failed attempt must count as friction"
        );
    }

    #[test]
    fn parser_reads_the_three_block_types() {
        let answer = "SKILL_CREATE: vite-dev-server\n\
                      description: Start a Vite dev server and verify it.\n\
                      When the user wants a live preview:\n\
                      - `npm run dev -- --host 127.0.0.1` in the background\n\
                      - verify with curl\n\n\
                      SKILL_UPDATE: rijo-rust-harness\n\
                      Terminal `background: true` returns in ~2 ms; never use `&`.\n\n\
                      MEMORY: User prefers German answers in the CLI.\n";
        let actions = parse_actions(answer, 3);
        assert_eq!(actions.len(), 3, "{actions:?}");
        match &actions[0] {
            Action::CreateSkill {
                name,
                description,
                body,
            } => {
                assert_eq!(name, "vite-dev-server");
                assert!(description.starts_with("Start a Vite"));
                assert!(body.contains("npm run dev"));
            }
            other => panic!("expected CreateSkill, got {other:?}"),
        }
        assert!(
            matches!(&actions[1], Action::UpdateSkill { name, .. } if name == "rijo-rust-harness")
        );
        assert!(matches!(&actions[2], Action::Memory { text } if text.contains("German")));
    }

    #[test]
    fn parser_ignores_garbage_and_enforces_limits() {
        assert!(parse_actions("NOTHING", 3).is_empty());
        assert!(parse_actions("Sure, here is my analysis: everything was fine.", 3).is_empty());
        // Too many blocks are capped.
        let many = "MEMORY: a\nMEMORY: b\nMEMORY: c\nMEMORY: d\n";
        assert_eq!(parse_actions(many, 2).len(), 2);
        // Name paths are discarded.
        let evil = "SKILL_CREATE: ../../etc/passwd\ndescription: x\nbody\n";
        assert!(parse_actions(evil, 3).is_empty());
    }

    #[test]
    fn unsafe_names_are_rejected() {
        assert!(safe_skill_name("good-skill").is_some());
        assert!(safe_skill_name("../evil").is_none());
        assert!(safe_skill_name("a/b").is_none());
        assert!(safe_skill_name("").is_none());
        assert!(safe_skill_name("-start").is_none());
        assert!(safe_skill_name(&"x".repeat(65)).is_none());
    }

    #[test]
    fn apply_writes_skill_and_memory_only_into_our_own_stores() {
        let tmp = std::env::temp_dir().join(format!("rijo_learn_test_{}", std::process::id()));
        let skills = tmp.join("skills");
        let data = tmp.join("data");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::create_dir_all(&data).unwrap();

        let actions = vec![
            Action::CreateSkill {
                name: "demo-skill".into(),
                description: "Does a demo thing.".into(),
                body: "Use it when you demo.".into(),
            },
            Action::Memory {
                text: "User builds websites with Rijo Code.".into(),
            },
            Action::UpdateSkill {
                name: "does-not-exist".into(),
                text: "Lesson".into(),
            },
        ];
        let applied = apply_actions(&actions, &skills, &data, 2200);
        assert!(
            applied.iter().any(|a| a.kind == "skill-create"),
            "create missing"
        );
        assert!(applied.iter().any(|a| a.kind == "memory"), "memory missing");
        assert!(
            applied.iter().any(|a| a.kind == "skipped"),
            "skip note missing"
        );

        let created = std::fs::read_to_string(skills.join("demo-skill").join("SKILL.md")).unwrap();
        assert!(created.contains("name: demo-skill"));
        assert!(created.contains("source: turn-review"));
        let mem = std::fs::read_to_string(data.join("memories").join("MEMORY.md")).unwrap();
        assert!(mem.contains("builds websites with Rijo Code"));

        // Second run: create becomes a lesson, memory is not duplicated.
        let applied2 = apply_actions(&actions, &skills, &data, 2200);
        assert!(
            applied2.iter().any(|a| a.kind == "skill-update"),
            "{:?}",
            applied2.iter().map(|a| a.kind).collect::<Vec<_>>()
        );
        let mem2 = std::fs::read_to_string(data.join("memories").join("MEMORY.md")).unwrap();
        assert_eq!(
            mem2.matches("builds websites with Rijo Code").count(),
            1,
            "duplicate in memory"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
