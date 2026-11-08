//! `brain_outline` — the structural map of a file or directory.
//!
//! `brain_search` answers "where is something"; this answers "what is in here"
//! without sending file bodies: names, kinds, line numbers and signatures.
//! The same extractor feeds the symbol nodes in the brain index, so both views
//! agree.

use crate::brain::outline;
use crate::tools::{file_tools, tool_error, JsonSchema, Tool, ToolMeta, ToolResult};
use crate::AppState;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

pub struct BrainOutline;

const MAX_FILES: usize = 80;
const MAX_SYMBOLS: usize = 300;
const MAX_FILE_BYTES: u64 = 512 * 1024;

fn supports(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "rs" | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "py"
            | "html"
            | "htm"
            | "svg"
            | "md"
            | "markdown"
    )
}

impl Tool for BrainOutline {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "brain_outline",
            description: "Structural map of a file or directory: functions, classes, sections and headings with line numbers and signatures, without file bodies. Use it before read_file on unfamiliar code.",
            emoji: "🧭",
            toolset: "brain",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let raw_path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        let max_files = args
            .get("max_files")
            .and_then(|v| v.as_u64())
            .unwrap_or(40)
            .clamp(1, MAX_FILES as u64) as usize;

        let workspace = state.config.workspace.clone();
        let root = file_tools::resolve_path(raw_path, &workspace);
        if !root.exists() {
            return tool_error(format!("path does not exist: {}", root.display()));
        }

        let mut files: Vec<String> = if root.is_file() {
            vec![root.to_string_lossy().to_string()]
        } else {
            let mut walked = file_tools::walk_files(root.to_string_lossy().as_ref())
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| supports(Path::new(entry)))
                .collect::<Vec<_>>();
            walked.sort();
            walked
        };

        let mut entries = Vec::new();
        let mut symbol_total = 0usize;
        let mut truncated = false;

        for file in files.drain(..) {
            if entries.len() >= max_files || symbol_total >= MAX_SYMBOLS {
                truncated = true;
                break;
            }
            let path = Path::new(&file);
            if std::fs::metadata(path)
                .map(|meta| meta.len() > MAX_FILE_BYTES)
                .unwrap_or(true)
            {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            let symbols: Vec<Value> = outline::extract(path, &content)
                .into_iter()
                .filter(|symbol| {
                    pattern
                        .as_ref()
                        .map(|needle| symbol.name.to_ascii_lowercase().contains(needle))
                        .unwrap_or(true)
                })
                .take(MAX_SYMBOLS.saturating_sub(symbol_total))
                .map(|symbol| {
                    json!({
                        "name": symbol.name,
                        "kind": symbol.kind,
                        "line": symbol.line,
                        "signature": symbol.signature,
                    })
                })
                .collect();
            if symbols.is_empty() {
                continue;
            }
            symbol_total += symbols.len();
            let shown = path
                .strip_prefix(&workspace)
                .map(|relative| relative.to_string_lossy().to_string())
                .unwrap_or(file);
            entries.push(json!({ "path": shown, "symbols": symbols }));
        }

        json!({
            "file_count": entries.len(),
            "symbol_count": symbol_total,
            "truncated": truncated,
            "files": entries,
        })
        .to_string()
    }
}

lazy_static::lazy_static! {
    static ref SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File or directory inside the workspace (default '.')"
            },
            "pattern": {
                "type": "string",
                "description": "Only symbols whose name contains this text"
            },
            "max_files": {
                "type": "integer",
                "description": "Maximum files to outline (default 40, max 80)"
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::supports;
    use std::path::Path;

    #[test]
    fn only_outlineable_files_are_mapped() {
        assert!(supports(Path::new("a.rs")));
        assert!(supports(Path::new("a.TS")));
        assert!(supports(Path::new("index.html")));
        assert!(!supports(Path::new("style.css")));
        assert!(!supports(Path::new("data.bin")));
    }
}
