//! File tools — read, write, patch, and search files
//!
//! Drop-in replacements for Hermes' file_tools.py with native Rust speed.

use crate::tools::{tool_error, JsonSchema, Tool, ToolMeta, ToolResult};
use crate::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

const SEARCH_OUTPUT_BYTES: usize = 1_000_000;
const SEARCH_ERROR_BYTES: usize = 16_000;

/// Ranges already returned in this process, per file.
///
/// Re-reading a range the model already has in context costs a round-trip and
/// re-sends the same text. Measured 2026-10-17: 10 of 37 calls in one repo
/// analysis were such slices (`produkte.js` ×5, `sortiment.html` ×6).
struct ReadCoverage {
    mtime: u128,
    total: usize,
    ranges: Vec<(usize, usize)>,
}

static READ_COVERAGE: LazyLock<Mutex<HashMap<String, ReadCoverage>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn file_mtime(path: &str) -> u128 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|delta| delta.as_nanos())
        .unwrap_or(0)
}

fn covered_window(
    path: &str,
    offset: usize,
    limit: usize,
    mtime: u128,
) -> Option<(usize, usize, usize)> {
    if mtime == 0 {
        return None;
    }
    let coverage = READ_COVERAGE.lock().unwrap_or_else(|e| e.into_inner());
    let entry = coverage.get(path)?;
    if entry.mtime != mtime {
        return None;
    }
    let end = if limit == usize::MAX {
        entry.total
    } else {
        offset
            .saturating_add(limit)
            .saturating_sub(1)
            .min(entry.total)
    };
    entry
        .ranges
        .iter()
        .copied()
        .find(|(start, stop)| offset >= *start && offset <= *stop && end <= *stop)
        .map(|(start, stop)| (start, stop, entry.total))
}

fn record_window(path: &str, offset: usize, lines: usize, total: usize, mtime: u128) {
    if lines == 0 || mtime == 0 {
        return;
    }
    let start = offset.max(1);
    let end = start.saturating_add(lines).saturating_sub(1);
    let mut coverage = READ_COVERAGE.lock().unwrap_or_else(|e| e.into_inner());
    let entry = coverage
        .entry(path.to_string())
        .or_insert_with(|| ReadCoverage {
            mtime,
            total,
            ranges: Vec::new(),
        });
    if entry.mtime != mtime {
        entry.mtime = mtime;
        entry.total = total;
        entry.ranges.clear();
    }
    entry.total = total;
    if let Some(last) = entry.ranges.last_mut() {
        if start <= last.1.saturating_add(1) && end >= last.0 {
            last.0 = last.0.min(start);
            last.1 = last.1.max(end);
            return;
        }
    }
    entry.ranges.push((start, end));
}

/// Resolve a tool path against the workspace, never the process cwd.
///
/// The CLI normally runs with cwd == workspace. The desktop host does not: one
/// process serves several projects, so a relative path used to resolve against
/// the directory Rijo was launched from. Measured 2026-10-17: a run started in
/// an empty `test2` workspace analysed the Rijo checkout instead, because every
/// relative read/search followed the launch directory.
pub(crate) fn resolve_path(raw: &str, workspace: &std::path::Path) -> std::path::PathBuf {
    let expanded = shellexpand::tilde(raw).to_string();
    if expanded == "." {
        return workspace.to_path_buf();
    }
    let path = std::path::PathBuf::from(&expanded);
    if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    }
}

struct BoundedCommandOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

/// Drain both pipes continuously while retaining only bounded prefixes. Once
/// either budget is exceeded, stop the search process rather than buffering an
/// unbounded `Command::output()` result and truncating it afterwards.
fn output_capped(command: &mut std::process::Command) -> std::io::Result<BoundedCommandOutput> {
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("stdout pipe requested");
    let stderr = child.stderr.take().expect("stderr pipe requested");
    let exceeded = Arc::new(AtomicBool::new(false));

    let read_prefix = |mut stream: Box<dyn Read + Send>, cap: usize, exceeded: Arc<AtomicBool>| {
        std::thread::spawn(move || {
            let mut kept = Vec::with_capacity(cap.min(64 * 1024));
            let mut buffer = [0u8; 16 * 1024];
            loop {
                let n = match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let keep = n.min(cap.saturating_sub(kept.len()));
                kept.extend_from_slice(&buffer[..keep]);
                if keep < n {
                    exceeded.store(true, Ordering::Relaxed);
                }
            }
            kept
        })
    };

    let stdout_task = read_prefix(Box::new(stdout), SEARCH_OUTPUT_BYTES, exceeded.clone());
    let stderr_task = read_prefix(Box::new(stderr), SEARCH_ERROR_BYTES, exceeded.clone());
    let status = loop {
        if exceeded.load(Ordering::Relaxed) {
            let _ = child.kill();
            break child.wait()?;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let stdout = stdout_task.join().unwrap_or_default();
    let stderr = stderr_task.join().unwrap_or_default();
    Ok(BoundedCommandOutput {
        status,
        stdout,
        stderr,
        truncated: exceeded.load(Ordering::Relaxed),
    })
}

/// Register a path as "read" (read-before-edit invariant).
fn mark_read(state: &Arc<AppState>, path: &str) {
    if let Ok(mut set) = state.files_read.lock() {
        set.insert(path.to_string());
    }
}

/// Stream a file and return `(window lines for offset/limit, total line count,
/// byte size)`.
///
/// Why not `read_to_string`: paging a 10-line window out of a huge log used to
/// read the WHOLE file into one String. This holds one line at a time, so memory
/// is bounded by the window, not the file. Non-UTF-8 bytes are replaced
/// (`from_utf8_lossy`) instead of erroring the read.
fn read_window(
    path: &str,
    offset: usize,
    limit: usize,
) -> std::io::Result<(Vec<String>, usize, u64)> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut reader = std::io::BufReader::new(file);
    let skip = offset.saturating_sub(1);
    let mut buf: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut window: Vec<String> = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        if total >= skip && window.len() < limit {
            let mut line = String::from_utf8_lossy(&buf).to_string();
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            window.push(line);
        }
        total += 1;
    }
    Ok((window, total, size))
}

/// Write `content` to `path` atomically: write a sibling temp file, then rename
/// it over the target. A reader (or a crash) therefore sees either the complete
/// old file or the complete new one — never a half-written file. Replacing in
/// the same directory keeps the rename on one filesystem (rename is atomic
/// there). The original file mode (e.g. an executable bit) is preserved.
fn atomic_write(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(".{name}.rijo-tmp-{}-{nonce}", std::process::id()));
    std::fs::write(&tmp, content)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Never leave the temp file behind; never remove the original.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Find any secret `.env` file inside a directory tree (used before a recursive
/// delete, so "NEVER deletes .env files" holds for trees too). Hidden entries
/// are walked on purpose — `.env` is hidden.
fn find_env_in_tree(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if crate::trust::is_env_file(&p) {
                return Some(p);
            }
            // remove_dir_all does not follow a directory symlink; the safety
            // scan must match that behavior and must not recurse into cycles.
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(p);
            }
        }
    }
    None
}

/// Has this file been read this session? (canon-path tolerant)
fn was_read(state: &Arc<AppState>, path: &str) -> bool {
    if let Ok(set) = state.files_read.lock() {
        if set.contains(path) {
            return true;
        }
        // compare canonical forms
        if let (Ok(a), Ok(_b)) = (
            std::fs::canonicalize(path),
            std::fs::canonicalize(state.config.workspace.clone()),
        ) {
            for readp in set.iter() {
                if let Ok(rp) = std::fs::canonicalize(readp) {
                    if rp == a {
                        return true;
                    }
                }
            }
        }
    }
    false
}

pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "read_file",
            description: "Read a text file with line numbers and pagination.",
            emoji: "📖",
            toolset: "file",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &READ_FILE_SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let state_ref = &state;
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return tool_error("path is required"),
        };
        let path_buf = resolve_path(path, &state_ref.config.workspace);
        let path = path_buf.to_string_lossy().to_string();

        // NEVER expose .env file contents to the LLM
        if crate::trust::is_env_file(&path_buf) {
            return tool_error("Refusing to read .env files (key protection policy). Use `env` commands in Rijo Code to manage secrets.");
        }

        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let mut limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(350) as usize;

        // Small files are read whole. Slicing a 200-line file into 20-line
        // windows costs one round-trip per slice; the whole file is one call
        // and a few thousand tokens. 24 KB ≈ 400 lines of prose or code.
        if limit < 200 {
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.len() <= 24 * 1024 {
                    limit = usize::MAX;
                }
            }
        }

        let mtime = file_mtime(&path);
        if let Some((start, end, total)) = covered_window(&path, offset, limit, mtime) {
            return json!({
                "already_read": true,
                "path": path,
                "covered": format!("{start}-{end}"),
                "total_lines": total,
                "note": "This range is already read: it is in your context from an earlier read in \
                         this session. Do not read it again — use the earlier content. A file \
                         changed on disk is re-read automatically."
            })
            .to_string();
        }

        let (window, total_lines, file_size) = match read_window(&path, offset, limit) {
            Ok(v) => v,
            Err(e) => return tool_error(format!("Cannot read file: {e}")),
        };
        record_window(&path, offset, window.len(), total_lines, mtime);

        // Coverage is checked above, with the file's mtime: a range already
        // returned is not resent, but a changed file is always read again.

        let line_count = window.len();
        let numbered: String = window
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}|{}", offset + i, line))
            .collect::<Vec<_>>()
            .join("\n");

        // More lines remain if the streamed total exceeds the lines covered so far
        // (skipped + shown). `saturating_sub` keeps it panic-free for offset 0.
        let truncated = total_lines > offset.saturating_sub(1) + line_count;

        let already_read = was_read(state_ref, &path);
        mark_read(state_ref, &path);
        json!({
            "content": numbered,
            "total_lines": total_lines,
            "file_size": file_size,
            "offset": offset,
            "limit": if limit == usize::MAX { line_count } else { limit },
            "truncated": truncated,
            "already_read": already_read,
            "next_offset": if truncated { Some(offset + line_count) } else { None }
        })
        .to_string()
    }
}

pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "write_file",
            description: "Write a file (new or full rewrite). Creates parent directories. For edits use `patch`.",
            emoji: "✍️",
            toolset: "file",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &WRITE_FILE_SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return tool_error("path is required"),
        };
        let path_buf = resolve_path(path, &state.config.workspace);
        let path = path_buf.to_string_lossy().to_string();

        // File writes are workspace-scoped after tilde expansion. The expanded
        // path, not its original spelling, is the authorization decision.
        if !crate::trust::is_in_workspace(&path_buf, &state.config.workspace) {
            return tool_error(format!(
                "Cannot write outside workspace: {} is not under {}",
                path,
                state.config.workspace.display()
            ));
        }
        if crate::trust::is_env_file(&path_buf) {
            return tool_error("Refusing to write .env files (key protection policy)");
        }
        // Read-before-edit (same invariant as `patch`): overwriting an EXISTING
        // file the model has not read is a blind mutation. The README promises
        // this is rejected — now it is.
        if path_buf.exists() && !was_read(&state, &path) {
            return tool_error(format!(
                "File not read this session: {path}. Call read_file first — overwriting \
                 unseen content is rejected to prevent blind mutations. Use `patch` for edits."
            ));
        }

        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return tool_error("content is required"),
        };

        // Token-efficiency guard: big rewrite of an EXISTING file → hint patch
        let bytes = content.len();
        let big_rewrite_note = if path_buf.exists() && bytes > 8_000 {
            Some(format!(
                "note: {} bytes in one call — for smaller deltas prefer `patch` (fewer tokens, reviewable diff)",
                bytes
            ))
        } else {
            None
        };

        // Create parent directories
        if let Some(parent) = path_buf.parent() {
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return tool_error(format!("Cannot create parent directories: {e}"));
                }
            }
        }

        match atomic_write(&path_buf, content) {
            Ok(()) => {
                mark_read(&state, &path); // content now known — edit-eligible
                let mut out = json!({
                    "bytes_written": bytes,
                    "path": path,
                    "verified": true,
                    "message": format!("Written {bytes} bytes to {path}")
                });
                if let Some(note) = big_rewrite_note {
                    out["note"] = json!(note);
                }
                // Diagnostics feedback: attach compiler errors directly to the
                // result instead of making the model ask again one iteration
                // later (opt-in via diagnostics.after_edit).
                if let Some(diag) = crate::diagnostics::run(
                    &state.config.diagnostics,
                    &state.config.workspace,
                    &path_buf,
                ) {
                    out["diagnostics"] = diag;
                }
                out.to_string()
            }
            Err(e) => tool_error(format!("Cannot write file: {e}")),
        }
    }
}

pub struct PatchTool;

impl Tool for PatchTool {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "patch",
            description: "Targeted find-and-replace in files (fuzzy matching). PREFERRED for edits — sends only the delta. Multiple independent changes in one call: pass `edits: [{old_string,new_string}]` (all-or-nothing). Use instead of sed/awk/write_file.",
            emoji: "🔧",
            toolset: "file",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &PATCH_SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return tool_error("path is required"),
        };

        let path_buf = resolve_path(path, &state.config.workspace);
        let path = path_buf.to_string_lossy().to_string();
        if !crate::trust::is_in_workspace(&path_buf, &state.config.workspace) {
            return tool_error(format!(
                "Cannot patch outside workspace: {} is not under {}",
                path,
                state.config.workspace.display()
            ));
        }
        if crate::trust::is_env_file(&path_buf) {
            return tool_error("Refusing to patch .env files (key protection policy)");
        }

        // ── Read-before-edit invariant (Claude-Code pattern) ─────────────
        if !was_read(&state, &path) {
            return tool_error(format!(
                "File not read this session: {path}. Call read_file first — \
                 edits on unseen content are rejected to prevent blind mutations."
            ));
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return tool_error(format!("Cannot read file {path}: {e}")),
        };

        // Two shapes: one edit (`old_string`/`new_string`) or a batch
        // (`edits: [{old_string, new_string, replace_all?}]`). The batch is
        // all-or-nothing: if any block fails to match, NOTHING is written — so a
        // multi-block refactor can never land half-applied.
        let (new_content, count) = if let Some(edits) = args.get("edits").and_then(|v| v.as_array())
        {
            if edits.is_empty() {
                return tool_error(
                    "edits is empty — provide at least one {old_string, new_string}",
                );
            }
            let total_edits = edits.len();
            let mut working = content.clone();
            let mut applied = 0usize;
            for (i, edit) in edits.iter().enumerate() {
                let old = match edit.get("old_string").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None => return tool_error(format!("edits[{i}].old_string is required")),
                };
                let new = edit
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let ra = edit
                    .get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                match apply_one_edit(&working, old, new, ra) {
                    Ok((next, n)) => {
                        working = next;
                        applied += n;
                    }
                    Err(msg) => {
                        return tool_error(format!(
                            "edits[{i}] failed: {msg} — no change written \
                                 (all {total_edits} edits are atomic)."
                        ))
                    }
                }
            }
            (working, applied)
        } else {
            let old_string = match args.get("old_string").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => {
                    return tool_error(
                        "old_string is required (or pass `edits` for a batch of edits)",
                    )
                }
            };
            let new_string = args
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let replace_all = args
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            match apply_one_edit(&content, old_string, new_string, replace_all) {
                Ok(v) => v,
                Err(msg) => return tool_error(msg),
            }
        };

        match atomic_write(std::path::Path::new(&path), &new_content) {
            Ok(()) => {
                mark_read(&state, &path);
                let mut out = json!({
                    "path": path,
                    "replacements": count,
                    "message": format!("Applied {count} replacement(s) to {path}")
                });
                if let Some(diag) = crate::diagnostics::run(
                    &state.config.diagnostics,
                    &state.config.workspace,
                    std::path::Path::new(&path),
                ) {
                    out["diagnostics"] = diag;
                }
                out.to_string()
            }
            Err(e) => tool_error(format!("Cannot write patched file: {e}")),
        }
    }
}

/// Apply ONE edit to `content` with the tiered matcher and return the new
/// content plus the number of replacements, or an actionable error.
///
/// Used both for a single `patch` call and for each entry of an `edits` batch.
fn apply_one_edit(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<(String, usize), String> {
    if old_string == new_string {
        return Err("old_string and new_string are identical — no-op".into());
    }

    // ── Uniqueness enforcement before any match attempt ─────────────
    let occurrences = content.matches(old_string).count();
    if !replace_all && occurrences > 1 {
        return Err(format!(
            "old_string matches {occurrences} locations — not unique. \
             Add surrounding lines to pin ONE spot, or set replace_all=true."
        ));
    }

    // ── Tiered fuzzy matching (Codex-style 3 levels) ─────────────────
    if occurrences > 0 {
        // Tier 1: exact
        if replace_all {
            return Ok((content.replace(old_string, new_string), occurrences));
        }
        let pos = content.find(old_string).unwrap();
        return Ok((
            content[..pos].to_string() + new_string + &content[pos + old_string.len()..],
            1,
        ));
    }

    // Tier 2: line-ending tolerant (\r\n vs \n)
    let old_le = old_string.replace("\r\n", "\n");
    let unified = content.replace("\r\n", "\n");
    let count_le = create_unified_matches(&unified, &old_le);
    if count_le > 0 {
        let replaced = if replace_all {
            unified.replace(&old_le, new_string)
        } else {
            let pos = unified.find(&old_le).unwrap();
            unified[..pos].to_string() + new_string + &unified[pos + old_le.len()..]
        };
        return Ok((replaced, count_le));
    }

    // Tier 3: whitespace-insensitive per-line match
    match tier3_ws_match(content, old_string, new_string, replace_all) {
        Some(out) => Ok(out),
        None => Err(fuzzy_fail_hint(content, old_string)),
    }
}

/// Tier 2 helper: count normalized matches (name kept for clarity).
fn create_unified_matches(content: &str, old: &str) -> usize {
    content.matches(old).count()
}

/// Tier 3: whitespace-insensitive line-block matching. Handles the common
/// "differs only in indentation/spacing" case WITHOUT re-indenting the output
/// (replacement is inserted as-is).
fn tier3_ws_match(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Option<(String, usize)> {
    let old_trim: Vec<&str> = old.trim().lines().map(|l| l.trim()).collect();
    if old_trim.is_empty() {
        return None;
    }
    let lines: Vec<&str> = content.lines().collect();
    let n = old_trim.len();
    if lines.len() < n {
        return None;
    }

    let mut starts: Vec<usize> = Vec::new();
    let mut w = 0usize;
    while w + n <= lines.len() {
        if (lines[w..w + n].iter().map(|l| l.trim())).eq(old_trim.iter().copied()) {
            starts.push(w);
            w += n; // non-overlapping
        } else {
            w += 1;
        }
    }
    if starts.is_empty() {
        return None;
    }
    if !replace_all && starts.len() > 1 {
        return None;
    }

    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    let mut count = 0usize;
    for &s in &starts {
        out.extend(lines[i..s].iter().map(|l| l.to_string()));
        let indent_len = lines[s].len() - lines[s].trim_start().len();
        let indent: String = lines[s][..indent_len].to_string();
        for nl in new.trim().lines() {
            if nl.trim().is_empty() {
                out.push(String::new());
            } else {
                out.push(format!("{indent}{}", nl.trim_start()));
            }
        }
        i = s + n;
        count += 1;
    }
    out.extend(lines[i..].iter().map(|l| l.to_string()));
    if content.ends_with('\n') {
        // preserve trailing newline contract of fs::read/write
        let mut s = out.join("\n");
        s.push('\n');
        Some((s, count))
    } else {
        Some((out.join("\n"), count))
    }
}

/// Actionable error on fuzzy failure — tells the model exactly what to do next.
fn fuzzy_fail_hint(content: &str, old_string: &str) -> String {
    let probe: String = old_string
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(40)
        .collect();
    if probe.is_empty() {
        return "old_string is empty/blank and not found. Provide the real block from read_file output.".into();
    }
    let mut best: Option<(f64, usize)> = None;
    let pl: Vec<char> = probe.chars().collect();
    for (i, line) in content.lines().enumerate() {
        let lshort: Vec<char> = line.trim().chars().take(pl.len()).collect();
        if lshort.is_empty() {
            continue;
        }
        let hits = pl.iter().filter(|c| lshort.contains(c)).count();
        let sim = hits as f64 / pl.len() as f64;
        if best.as_ref().map(|(s, _)| sim > *s).unwrap_or(true) && sim > 0.5 {
            best = Some((sim, i + 1));
        }
    }
    match best {
        Some((_, line)) => format!(
            "old_string not found (exact, line-endings and whitespace tiers failed). \
             Closest line: {line}. Take EXACT lines from a fresh read_file call \
             (strip the 'N|' prefixes) and re-issue the patch."
        ),
        None => "old_string not found in file. Use search_files to locate the current text, then patch with exact content.".into(),
    }
}

pub struct DeleteFileTool;

impl Tool for DeleteFileTool {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "delete_file",
            description:
                "Delete a file (or, with `recursive: true`, a directory tree) in the workspace. Requires confirmation. NEVER deletes .env files.",
            emoji: "🗑️",
            toolset: "file",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &DELETE_FILE_SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return tool_error("path is required"),
        };
        let path = resolve_path(path, &state.config.workspace);

        // NEVER delete .env files
        if crate::trust::is_env_file(&path) {
            return tool_error("Refusing to delete .env files (security policy)");
        }

        // Must be inside workspace
        if !crate::trust::is_in_workspace(&path, &state.config.workspace) {
            return tool_error(format!(
                "Cannot delete outside workspace: {} is not under {}",
                path.display(),
                state.config.workspace.display()
            ));
        }

        if !path.exists() {
            return tool_error(format!("Path not found: {}", path.display()));
        }

        let recursive = args
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let is_dir = path.is_dir();

        let action = if is_dir {
            if !recursive {
                return tool_error(format!(
                    "{} is a directory. Deleting a directory tree requires `recursive: true`.",
                    path.display()
                ));
            }
            // A tree delete must not become a back door around the .env policy.
            if let Some(env) = find_env_in_tree(&path) {
                return tool_error(format!(
                    "Refusing recursive delete: the tree contains {} — .env files are never deleted.",
                    env.display()
                ));
            }
            "directory tree deletion"
        } else if path.is_file() {
            "file deletion"
        } else {
            return tool_error(format!(
                "Not a regular file or directory: {}",
                path.display()
            ));
        };

        // Approval through the central gate: TUI dialog, headless fail-closed.
        // The text is the same as for terminal/execute_code so that the model
        // does not misread a denial as "try another tool".
        if let Err(msg) = crate::guard::approve_blocking(
            &state,
            "delete_file",
            &path.display().to_string(),
            action,
        ) {
            return tool_error(msg);
        }

        let result = if is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => json!({
                "deleted": true,
                "path": path.to_string_lossy(),
                "recursive": is_dir,
                "message": format!("Deleted {}", path.display())
            })
            .to_string(),
            Err(e) => tool_error(format!("Cannot delete {}: {e}", path.display())),
        }
    }
}

pub struct SearchFilesTool;

impl Tool for SearchFilesTool {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "search_files",
            description:
                "Search file contents (regex) or find files by glob. Use instead of grep/find/ls. With target 'files' and output_mode 'details' it also returns size and line count per file — use that instead of a `find | wc -c` shell loop.",
            emoji: "🔎",
            toolset: "file",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &SEARCH_FILES_SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return tool_error("pattern is required"),
        };

        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("content");
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let output = args
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("content");
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
        let case_sensitive = args
            .get("case_sensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let include = args.get("include").and_then(|v| v.as_str());

        let resolved = resolve_path(path, &state.config.workspace)
            .to_string_lossy()
            .to_string();

        match target {
            "content" => {
                let resolved_path = std::path::Path::new(&resolved);
                if crate::trust::is_env_file(resolved_path) {
                    return tool_error("Refusing to search .env files (key protection policy)");
                }
                // Use ripgrep if available, otherwise fallback to our own grep
                let mut cmd = std::process::Command::new("rg");
                // Ignore user ripgrep config: a personal `--hidden` setting
                // must not turn recursive search into a `.env` reader.
                cmd.arg("--no-config").arg("--color").arg("never");
                if !case_sensitive {
                    cmd.arg("-i");
                }
                if let Some(glob) = include {
                    cmd.arg("--glob").arg(glob);
                }
                match output {
                    // Advertised in the schema but previously fell through to
                    // content: emit one path per matching file.
                    "files_only" => {
                        cmd.arg("--files-with-matches");
                    }
                    "count" => {
                        cmd.arg("--count-matches");
                    }
                    _ => {
                        cmd.arg("--line-number")
                            .arg("--with-filename")
                            .arg("--max-count")
                            .arg(limit.to_string());
                    }
                }
                cmd.arg(pattern).arg(&resolved);

                match output_capped(&mut cmd) {
                    Ok(out) if out.status.success() || out.truncated => {
                        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                        // rg's --max-count is per-file; enforce a hard total for
                        // the result modes, and report when the byte cap stopped it.
                        let mut limited = if output == "count" && !out.truncated {
                            stdout
                        } else {
                            stdout.lines().take(limit).collect::<Vec<_>>().join("\n")
                        };
                        let count = limited.lines().count();
                        if out.truncated {
                            limited.push_str("\n[Search output truncated at 1,000,000 bytes]");
                        }
                        json!({
                            "matches": limited,
                            "count": count,
                            "truncated": out.truncated
                        })
                        .to_string()
                    }
                    Ok(out) => {
                        let details = String::from_utf8_lossy(&out.stderr);
                        if details.contains("not found") || details.contains("No such") {
                            json!({"matches": "", "count": 0}).to_string()
                        } else {
                            json!({"matches": "", "count": 0, "warning": details.to_string()})
                                .to_string()
                        }
                    }
                    Err(_) => {
                        // Fallback: manual grep
                        self.fallback_grep(
                            pattern,
                            &resolved,
                            limit,
                            output,
                            case_sensitive,
                            include,
                        )
                    }
                }
            }
            "files" => {
                // Always the resolved root: `path == "."` used to fall back to
                // the process cwd, which made a workspace listing return the
                // launcher's repository (measured 2026-10-17).
                let glob_pattern = shellexpand::tilde(pattern).to_string();
                let mut results = Vec::new();
                let root = resolved.as_str();
                if let Ok(entries) = walk_files(root) {
                    let matcher = glob::Pattern::new(&glob_pattern).ok();
                    for entry in entries {
                        if results.len() >= limit {
                            break;
                        }
                        let matched = matcher
                            .as_ref()
                            .map(|candidate| candidate.matches(&entry))
                            .unwrap_or(false);
                        if matched {
                            results.push(entry);
                        }
                    }
                }

                // `details` replaces the shell inventory (`find … | wc -c`):
                // one call returns path, size and line count per file.
                if output == "details" {
                    let mut files: Vec<Value> = results
                        .iter()
                        .map(|entry| {
                            let bytes =
                                std::fs::metadata(entry).map(|meta| meta.len()).unwrap_or(0);
                            json!({
                                "path": entry,
                                "bytes": bytes,
                                "lines": count_lines(entry, bytes),
                            })
                        })
                        .collect();
                    files.sort_by(|left, right| {
                        right["bytes"]
                            .as_u64()
                            .unwrap_or(0)
                            .cmp(&left["bytes"].as_u64().unwrap_or(0))
                    });
                    let total: u64 = files
                        .iter()
                        .map(|file| file["bytes"].as_u64().unwrap_or(0))
                        .sum();
                    return json!({
                        "count": files.len(),
                        "total_bytes": total,
                        "files": files,
                        "truncated": results.len() >= limit,
                    })
                    .to_string();
                }

                json!({
                    "matches": results,
                    "count": results.len()
                })
                .to_string()
            }
            _ => tool_error(format!(
                "Unknown target: {target}. Use 'content' or 'files'."
            )),
        }
    }
}

impl SearchFilesTool {
    fn fallback_grep(
        &self,
        pattern: &str,
        path: &str,
        limit: usize,
        output: &str,
        case_sensitive: bool,
        include: Option<&str>,
    ) -> ToolResult {
        let mut results = Vec::new();
        let re = match regex::RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
        {
            Ok(r) => r,
            Err(e) => return tool_error(format!("Invalid regex pattern: {e}")),
        };
        let include_glob = include.and_then(|g| glob::Pattern::new(g).ok());
        // The ripgrep path is capped by `output_capped`; the fallback must enforce
        // the same byte budget, or a file with one enormous matching line returns
        // it whole (measured on a system without `rg`).
        let mut total_bytes = 0usize;
        let mut truncated = false;

        if let Ok(entries) = walk_files(path) {
            'files: for entry in entries {
                if results.len() >= limit {
                    break;
                }
                if let Some(g) = &include_glob {
                    if !g.matches(&entry) {
                        continue;
                    }
                }
                if crate::trust::is_env_file(std::path::Path::new(&entry)) {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&entry) {
                    for (line_no, line) in content.lines().enumerate() {
                        if !re.is_match(line) {
                            continue;
                        }
                        // One path per matching file.
                        if output == "files_only" {
                            if total_bytes + entry.len() + 1 > SEARCH_OUTPUT_BYTES {
                                truncated = true;
                                break 'files;
                            }
                            total_bytes += entry.len() + 1;
                            results.push(entry.clone());
                            break;
                        }
                        let rendered = match output {
                            "count" => format!("{}:{}", entry, line_no + 1),
                            _ => format!("{}:{}:{}", entry, line_no + 1, line),
                        };
                        if total_bytes + rendered.len() + 1 > SEARCH_OUTPUT_BYTES {
                            truncated = true;
                            break 'files;
                        }
                        total_bytes += rendered.len() + 1;
                        results.push(rendered);
                        if results.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }

        let mut matches = results.join("\n");
        if truncated {
            matches.push_str("\n[Search output truncated at 1,000,000 bytes]");
        }
        json!({
            "matches": matches,
            "count": results.len(),
            "truncated": truncated
        })
        .to_string()
    }
}

/// Line count for an inventory entry. `None` for binaries and very large files,
/// where reading would cost more than the number is worth.
fn count_lines(path: &str, bytes: u64) -> Option<u64> {
    const MAX_BYTES: u64 = 4 * 1024 * 1024;
    if bytes == 0 {
        return Some(0);
    }
    if bytes > MAX_BYTES {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    if data.iter().take(8192).any(|byte| *byte == 0) {
        return None;
    }
    let newlines = data.iter().filter(|byte| **byte == b'\n').count() as u64;
    Some(if data.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    })
}

pub(crate) fn walk_files(dir: &str) -> std::io::Result<Vec<String>> {
    let mut files = Vec::new();
    let path = std::path::Path::new(dir);
    if !path.is_dir() {
        files.push(dir.to_string());
        return Ok(files);
    }

    visit_dirs(path, &mut files)?;
    Ok(files)
}

/// Directories the fallback walker skips — mirrors ripgrep's defaults for the
/// common build/VCS output. (ripgrep, which is preferred, also honours
/// `.gitignore`; this fallback does not.)
fn is_skipped_dir(name: &str) -> bool {
    matches!(
        name,
        "node_modules" | "target" | "dist" | "build" | "vendor" | "__pycache__" | "coverage"
    )
}

fn visit_dirs(dir: &std::path::Path, files: &mut Vec<String>) -> std::io::Result<()> {
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            // Do not follow links outside the search tree or back into an
            // ancestor. A user can still search a path by passing it directly.
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                // Skip hidden dirs plus the usual build/VCS output. The fallback
                // runs only when `rg` is missing; it mirrors ripgrep's defaults
                // for the common cases. Full .gitignore parsing is NOT done here
                // (ripgrep, which is preferred, does that).
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !name.starts_with('.') && !is_skipped_dir(name) {
                        visit_dirs(&path, files)?;
                    }
                }
            } else {
                files.push(path.to_string_lossy().to_string());
            }
        }
    }
    Ok(())
}

lazy_static::lazy_static! {
    static ref READ_FILE_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File path (abs, rel, ~/path)"
            },
            "offset": {
                "type": "integer",
                "description": "First line (1-indexed)",
                "minimum": 1
            },
            "limit": {
                "type": "integer",
                "description": "Max lines (default 350, max 2000)"
            }
        },
        "required": ["path"]
    }));

    static ref WRITE_FILE_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File path (created/overwritten)"
            },
            "content": {
                "type": "string",
                "description": "Full content. Only for new files/rewrites — use `patch` for edits."
            }
        },
        "required": ["path", "content"]
    }));

    static ref PATCH_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File path to edit"
            },
            "old_string": {
                "type": "string",
                "description": "Text to find (single edit)"
            },
            "new_string": {
                "type": "string",
                "description": "Replacement text"
            },
            "replace_all": {
                "type": "boolean",
                "description": "All occurrences"
            },
            "edits": {
                "type": "array",
                "description": "Batch: several {old_string, new_string, replace_all?} applied all-or-nothing (use instead of old_string/new_string)",
                "items": {
                    "type": "object",
                    "properties": {
                        "old_string": { "type": "string" },
                        "new_string": { "type": "string" },
                        "replace_all": { "type": "boolean" }
                    },
                    "required": ["old_string", "new_string"]
                }
            }
        },
        "required": ["path"]
    }));

    static ref SEARCH_FILES_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Regex (content) or glob (files)"
            },
            "target": {
                "type": "string",
                "enum": ["content", "files"],
                "description": "content | files"
            },
            "path": {
                "type": "string",
                "description": "Dir (default .)"
            },
            "output_mode": {
                "type": "string",
                "enum": ["content", "files_only", "count", "details"],
                "description": "content (file:line:text) | files_only (paths) | count | details (target 'files': path, bytes, lines — use instead of `find | wc -c`)"
            },
            "case_sensitive": {
                "type": "boolean",
                "description": "Match case exactly (default false)"
            },
            "include": {
                "type": "string",
                "description": "Only search files matching this glob (e.g. '*.rs')"
            },
            "limit": {
                "type": "integer",
                "description": "Max results total (default: 50)"
            }
        },
        "required": ["pattern"]
    }));

    static ref DELETE_FILE_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File (or directory) path inside the workspace"
            },
            "recursive": {
                "type": "boolean",
                "description": "Required to delete a directory and its contents (default false)"
            }
        },
        "required": ["path"]
    }));
}

#[cfg(test)]
mod tests {
    use super::{resolve_path, SearchFilesTool};

    /// Relative tool paths must follow the workspace. Regression for the
    /// 2026-10-17 run: the desktop app was launched from the Rijo checkout, so
    /// `search_files "."` and `read_file "Cargo.toml"` in an empty `test2`
    /// workspace resolved against the launcher and analysed the wrong repo.
    #[test]
    fn relative_paths_resolve_against_the_workspace_not_the_process_cwd() {
        let workspace = std::path::Path::new("C:/work/test2");
        assert_eq!(
            resolve_path("Cargo.toml", workspace),
            workspace.join("Cargo.toml")
        );
        assert_eq!(
            resolve_path(".", workspace),
            workspace.join("."),
            "a bare '.' must mean the workspace"
        );
        assert_eq!(
            resolve_path("sub/dir/file.rs", workspace),
            workspace.join("sub/dir/file.rs")
        );
    }

    #[test]
    fn absolute_paths_stay_absolute() {
        let workspace = std::path::Path::new("C:/work/test2");
        let absolute = if cfg!(windows) {
            "C:/other/Cargo.toml"
        } else {
            "/other/Cargo.toml"
        };
        assert_eq!(
            resolve_path(absolute, workspace),
            std::path::PathBuf::from(absolute)
        );
    }

    #[test]
    fn details_report_size_and_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "a\nb\n").expect("write");
        let path = file.to_str().expect("path");
        assert_eq!(super::count_lines(path, 4), Some(2));

        std::fs::write(&file, "a\nb").expect("rewrite");
        assert_eq!(super::count_lines(path, 3), Some(2), "no trailing newline");

        assert_eq!(super::count_lines(path, 0), Some(0), "empty file");
        assert_eq!(
            super::count_lines(path, 8 * 1024 * 1024),
            None,
            "large files are not read"
        );
    }

    #[test]
    fn fallback_search_excludes_secret_env_files_but_keeps_templates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "MARKER_SECRET=hidden\n").unwrap();
        std::fs::write(
            dir.path().join(".env.local"),
            "MARKER_LOCAL_SECRET=hidden\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(".env.example"), "MARKER_TEMPLATE=visible\n").unwrap();

        let output = SearchFilesTool.fallback_grep(
            "MARKER_",
            dir.path().to_str().unwrap(),
            50,
            "content",
            true,
            None,
        );

        assert!(output.contains("MARKER_TEMPLATE=visible"), "{output}");
        assert!(!output.contains("MARKER_SECRET=hidden"), "{output}");
        assert!(!output.contains("MARKER_LOCAL_SECRET=hidden"), "{output}");
    }
}
