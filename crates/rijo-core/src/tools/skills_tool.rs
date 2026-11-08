//! Skills tools — list, view, and manage skills (procedural memory)
//!
//! Skills are markdown files with YAML frontmatter stored in ~/.rijo/skills/.

use crate::tools::{tool_error, JsonSchema, Tool, ToolMeta, ToolResult};
use crate::AppState;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// A visible skill (name, short description, category).
///
/// One source for `skills_list` AND the system prompt index — so the two
/// cannot drift apart.
#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub category: String,
    /// Lives in OUR OWN directory (Rijo Code skills, incl. learned ones) — not in
    /// a foreign, read-only library. Own ones come first in the index so
    /// that they do not fall out of the 40-entry window in large libraries.
    pub own: bool,
}

/// Collects all visible skills, deduplicated (first root wins) and
/// sorted.
pub fn list_skills(state: &AppState) -> Vec<SkillEntry> {
    let roots = skills_roots(state);
    let mut out: Vec<SkillEntry> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let own_dir = skills_dir(state);
    for base in &roots {
        let is_own_root = base == &own_dir;
        let Ok(entries) = std::fs::read_dir(base) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let skill_md = if path.is_dir() {
                path.join("SKILL.md")
            } else if path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
                path.clone()
            } else {
                continue;
            };
            if !skill_md.exists() {
                continue;
            }
            // Skill roots are explicit read boundaries. Do not let a symlinked
            // directory or SKILL.md pull arbitrary outside files into the
            // prompt index.
            if !crate::trust::is_in_workspace(&skill_md, base) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&skill_md) else {
                continue;
            };
            let (fm, _) = parse_frontmatter(&content);
            let name = fm
                .get("name")
                .and_then(|v| v.as_str())
                .or_else(|| skill_md.parent().and_then(|p| p.file_name()?.to_str()))
                .unwrap_or("unknown")
                .to_string();
            if !seen.insert(name.clone()) {
                continue;
            }
            let description = fm
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let category = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(|n| if n == "skills" { "general" } else { n })
                .unwrap_or("general")
                .to_string();
            out.push(SkillEntry {
                name,
                description,
                category,
                own: is_own_root,
            });
        }
    }
    // Own (also learned) skills first, then alphabetical — stable so that
    // the prompt prefix cache does not break.
    out.sort_by(|a, b| b.own.cmp(&a.own).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Renders the skill list for the system prompt (progressive disclosure as
/// with Hermes): name + short description, so the model KNOWS that a matching
/// skill exists and can load it via `skill_view`.
///
/// Why this is necessary: measured in A/B (2026-09-15) — without this index the
/// model in 4 of 4 runs never called `skills_list`/`skill_view`, so a
/// learned skill lay unused. The order is sorted and stable,
/// so the provider prefix cache does not break.
pub fn render_index(entries: &[SkillEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    const MAX_SKILLS: usize = 40;
    const MAX_DESC: usize = 80;
    // Own/learned skills first, then alphabetical. Sorting here (not
    // only in list_skills) so that the cap, independent of the input,
    // does not cut off anything important.
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| b.own.cmp(&a.own).then_with(|| a.name.cmp(&b.name)));
    let mut out = String::from(
        "## Skills (procedural memory — CHECK THIS FIRST)\n\
         Before starting a multi-step task, look for a matching skill here and load it\n\
         with `skill_view` (name): skills hold the exact commands, conventions and\n\
         pitfalls for that kind of work. Skip when nothing matches.\n",
    );
    for e in sorted.iter().take(MAX_SKILLS) {
        let desc: String = e.description.trim().chars().take(MAX_DESC).collect();
        if desc.is_empty() {
            out.push_str(&format!("- {}\n", e.name));
        } else {
            out.push_str(&format!("- {} — {}\n", e.name, desc));
        }
    }
    if entries.len() > MAX_SKILLS {
        out.push_str(&format!("- … {} more\n", entries.len() - MAX_SKILLS));
    }
    out
}

pub struct SkillsList;
pub struct SkillView;
pub struct SkillManage;

fn skills_dir(state: &AppState) -> PathBuf {
    state.config.skills_dir.clone()
}

/// All skill read roots (Rijo Code's own directory + additional roots,
/// e.g. the Hermes library). Order = priority when searching.
fn skills_roots(state: &AppState) -> Vec<PathBuf> {
    let mut roots = state.config.skill_roots.clone();
    if !roots.contains(&state.config.skills_dir) {
        roots.push(state.config.skills_dir.clone());
    }
    roots
}

/// Finds a skill in ALL roots (not only in our own directory).
fn find_skill_anywhere(state: &AppState, name: &str) -> Option<PathBuf> {
    if !is_safe_component(name) {
        return None;
    }
    for root in skills_roots(state) {
        if let Some(dir) = find_skill_dir(&root, name) {
            return Some(dir);
        }
    }
    None
}

/// Skill names and categories are single portable path components, never paths.
fn is_safe_component(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Resolve an existing linked file without allowing a skill symlink or `..`
/// component to leave the skill directory. Secret env files stay protected in
/// this reader just as they are in `read_file`.
fn skill_read_target(skill_dir: &std::path::Path, file: &str) -> Result<PathBuf, String> {
    let target = skill_dir.join(file);
    if !target.exists() {
        return Err(format!("File not found in skill: {}", target.display()));
    }
    if !crate::trust::is_in_workspace(&target, skill_dir) {
        return Err("Refusing to read a path outside the skill directory".into());
    }
    let resolved = target
        .canonicalize()
        .map_err(|e| format!("Cannot resolve skill file: {e}"))?;
    if crate::trust::is_env_file(&resolved) {
        return Err("Refusing to read .env files (key protection policy)".into());
    }
    Ok(resolved)
}

/// Mutating operations may only target a skill physically contained in our
/// writable skills root. Extra `skill_roots` are read-only libraries.
fn own_skill_dir(state: &AppState, name: &str) -> Result<PathBuf, String> {
    if !is_safe_component(name) {
        return Err("skill name must be a single safe path component".into());
    }
    let root = skills_dir(state);
    let dir = find_skill_dir(&root, name)
        .ok_or_else(|| format!("Skill not found in writable skills directory: {name}"))?;
    if !crate::trust::is_in_workspace(&dir, &root) {
        return Err("Refusing to mutate a skill outside the writable skills directory".into());
    }
    Ok(dir)
}

fn find_skill_dir(base: &PathBuf, name: &str) -> Option<PathBuf> {
    // Search: name directly, or category/name
    let direct = base.join(name);
    if direct.join("SKILL.md").exists()
        && crate::trust::is_in_workspace(&direct.join("SKILL.md"), base)
    {
        return Some(direct);
    }
    // Search categories
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skill_path = path.join(name);
                if skill_path.join("SKILL.md").exists()
                    && crate::trust::is_in_workspace(&skill_path.join("SKILL.md"), base)
                {
                    return Some(skill_path);
                }
                // Also check if this IS the skill (category=name)
                if path.join("SKILL.md").exists()
                    && path.file_name().and_then(|n| n.to_str()) == Some(name)
                    && crate::trust::is_in_workspace(&path.join("SKILL.md"), base)
                {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn parse_frontmatter(content: &str) -> (serde_json::Value, &str) {
    let content = content.trim();
    if let Some(rest) = content.strip_prefix("---") {
        if let Some(end) = rest.find("---") {
            let fm = &rest[..end];
            let body = rest[end + 3..].trim();
            if let Ok(val) = serde_yaml::from_str::<serde_json::Value>(fm) {
                return (val, body);
            }
        }
    }
    (json!({}), content)
}

impl Tool for SkillsList {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "skills_list",
            description: "List available skills.",
            emoji: "📋",
            toolset: "skills",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &SKILLS_LIST_SCHEMA
    }

    fn call(&self, _args: Value, state: Arc<AppState>) -> ToolResult {
        // Same source as the system prompt index: no drift possible.
        let skills: Vec<Value> = list_skills(&state)
            .into_iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": s.description,
                    "category": s.category,
                })
            })
            .collect();
        json!({ "skills": skills }).to_string()
    }
}

impl Tool for SkillView {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "skill_view",
            description: "Load a skill's content or linked files.",
            emoji: "📖",
            toolset: "skills",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &SKILL_VIEW_SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return tool_error("name is required"),
        };

        let file_path = args.get("file_path").and_then(|v| v.as_str());

        let skill_dir = match find_skill_anywhere(&state, name) {
            Some(d) => d,
            None => return tool_error(format!("Skill not found: {name}")),
        };

        let file = file_path.unwrap_or("SKILL.md");
        let target = match skill_read_target(&skill_dir, file) {
            Ok(p) => p,
            Err(e) => return tool_error(e),
        };

        match std::fs::read_to_string(&target) {
            Ok(content) => json!({
                "content": content,
                "path": target.to_string_lossy(),
            })
            .to_string(),
            Err(e) => tool_error(format!("Cannot read skill file: {e}")),
        }
    }
}

impl Tool for SkillManage {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "skill_manage",
            description: "Create/update/delete skills (procedural memory).",
            emoji: "🔧",
            toolset: "skills",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &SKILL_MANAGE_SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let action = match args.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => return tool_error("action is required (create/update/delete/patch)"),
        };

        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return tool_error("name is required"),
        };

        let base = skills_dir(&state);

        match action {
            "create" => {
                let content = match args.get("content").and_then(|v| v.as_str()) {
                    Some(c) => c,
                    None => return tool_error("content is required for create"),
                };

                let category = args
                    .get("category")
                    .and_then(|v| v.as_str())
                    .unwrap_or("general");

                if !is_safe_component(name) || !is_safe_component(category) {
                    return tool_error(
                        "skill name and category must be single safe path components",
                    );
                }

                if let Err(e) = std::fs::create_dir_all(&base) {
                    return tool_error(format!("Cannot create skills root: {e}"));
                }
                let root = match base.canonicalize() {
                    Ok(p) => p,
                    Err(e) => return tool_error(format!("Cannot resolve skills root: {e}")),
                };
                let category_dir = base.join(category);
                if std::fs::symlink_metadata(&category_dir)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
                    && !crate::trust::is_in_workspace(&category_dir, &root)
                {
                    return tool_error(
                        "Refusing to create a skill through an escaping category symlink",
                    );
                }
                if let Err(e) = std::fs::create_dir_all(&category_dir) {
                    return tool_error(format!("Cannot create skill category: {e}"));
                }
                if !crate::trust::is_in_workspace(&category_dir, &root) {
                    return tool_error(
                        "Refusing to create a skill outside the writable skills root",
                    );
                }

                let dir = category_dir.join(name);
                if let Err(e) = std::fs::create_dir(&dir) {
                    return tool_error(format!("Cannot create skill dir: {e}"));
                }
                if !crate::trust::is_in_workspace(&dir, &root) {
                    let _ = std::fs::remove_dir(&dir);
                    return tool_error(
                        "Refusing to create a skill outside the writable skills root",
                    );
                }

                let skill_path = dir.join("SKILL.md");
                let result = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&skill_path)
                    .and_then(|mut f| std::io::Write::write_all(&mut f, content.as_bytes()));
                if let Err(e) = result {
                    let _ = std::fs::remove_dir(&dir);
                    return tool_error(format!("Cannot write skill: {e}"));
                }

                json!({
                    "success": true,
                    "path": skill_path.to_string_lossy(),
                    "message": format!("Created skill {name} in category {category}")
                })
                .to_string()
            }
            "delete" => {
                let skill_dir = match own_skill_dir(&state, name) {
                    Ok(d) => d,
                    Err(e) => return tool_error(e),
                };
                if let Err(e) = std::fs::remove_dir_all(&skill_dir) {
                    return tool_error(format!("Cannot delete skill: {e}"));
                }

                json!({
                    "success": true,
                    "message": format!("Deleted skill {name}")
                })
                .to_string()
            }
            "patch" => {
                let old = match args.get("old_string").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None => return tool_error("old_string is required for patch"),
                };
                let new = args
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                let skill_dir = match own_skill_dir(&state, name) {
                    Ok(d) => d,
                    Err(e) => return tool_error(e),
                };

                let file = args
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("SKILL.md");
                let target = match skill_read_target(&skill_dir, file) {
                    Ok(p) => p,
                    Err(e) => return tool_error(e),
                };

                let content = match std::fs::read_to_string(&target) {
                    Ok(c) => c,
                    Err(e) => return tool_error(format!("Cannot read skill file: {e}")),
                };

                if let Some(pos) = content.find(old) {
                    let new_content =
                        content[..pos].to_string() + new + &content[pos + old.len()..];
                    if let Err(e) = std::fs::write(&target, &new_content) {
                        return tool_error(format!("Cannot write skill file: {e}"));
                    }
                    json!({
                        "success": true,
                        "message": format!("Patched skill {name}")
                    })
                    .to_string()
                } else {
                    tool_error(format!("Could not find old_string in skill {name}/{file}"))
                }
            }
            _ => tool_error(format!("Unknown action: {action}")),
        }
    }
}

lazy_static::lazy_static! {
    static ref SKILLS_LIST_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "category": {
                "type": "string",
                "description": "Optional category filter"
            }
        }
    }));

    static ref SKILL_VIEW_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Skill name"
            },
            "file_path": {
                "type": "string",
                "description": "Linked file path"
            }
        },
        "required": ["name"]
    }));

    static ref SKILL_MANAGE_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["create", "patch", "delete"],
                "description": "Action to perform"
            },
            "name": {
                "type": "string",
                "description": "Skill name"
            },
            "content": {
                "type": "string",
                "description": "SKILL.md content (create)"
            },
            "old_string": {
                "type": "string",
                "description": "Text to find (for patch)"
            },
            "new_string": {
                "type": "string",
                "description": "Replacement text (for patch)"
            },
            "category": {
                "type": "string",
                "description": "Category dir (create)"
            },
            "file_path": {
                "type": "string",
                "description": "File within skill (patch)"
            }
        },
        "required": ["action", "name"]
    }));
}

#[cfg(test)]
mod tests {
    use super::{render_index, SkillEntry};

    fn e(name: &str, description: &str) -> SkillEntry {
        SkillEntry {
            name: name.into(),
            description: description.into(),
            category: "general".into(),
            own: false,
        }
    }

    #[test]
    fn index_is_empty_without_skills() {
        assert_eq!(render_index(&[]), "");
    }

    #[test]
    fn index_lists_name_description_and_the_loading_hint() {
        let entries = vec![e("alpha-skill", "Does alpha things."), e("beta-skill", "")];
        let out = render_index(&entries);
        assert!(out.contains("## Skills"), "{out}");
        assert!(out.contains("skill_view"), "loading hint missing: {out}");
        assert!(out.contains("- alpha-skill — Does alpha things."), "{out}");
        // Without a description the line is still readable.
        assert!(out.contains("- beta-skill\n"), "{out}");
        // Order stays as passed (deterministic -> cache prefix).
        assert!(out.find("alpha-skill").unwrap() < out.find("beta-skill").unwrap());
    }

    #[test]
    fn own_skills_survive_the_cap() {
        // Foreign library with many entries: our own skill must stay in the
        // window, otherwise learned content is invisible again.
        let mut entries: Vec<SkillEntry> = (0..50)
            .map(|i| e(&format!("aaa{i:02}"), "foreign"))
            .collect();
        entries.push(SkillEntry {
            name: "zzz-learned".into(),
            description: "learned".into(),
            category: "general".into(),
            own: true,
        });
        let out = render_index(&entries);
        assert!(out.contains("zzz-learned"), "own skill was cut off: {out}");
    }

    #[test]
    fn index_caps_count_and_description_length() {
        let long = "x".repeat(300);
        let entries: Vec<SkillEntry> = (0..45).map(|i| e(&format!("s{i:02}"), &long)).collect();
        let out = render_index(&entries);
        let lines: Vec<&str> = out.lines().filter(|l| l.starts_with("- ")).collect();
        assert_eq!(lines.len(), 41, "40 skills + cap line expected:\n{out}");
        assert!(out.contains("5 more"), "cap must name the number: {out}");
        for line in lines.iter().take(40) {
            assert!(line.chars().count() < 100, "line too long: {line}");
        }
    }
}
