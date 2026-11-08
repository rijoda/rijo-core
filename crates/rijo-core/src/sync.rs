//! Machine-to-machine sync: the portable export/import core.
//!
//! Deliberately transport-agnostic. This module only knows how to write and read
//! a directory of mergeable files; a private git repo (or rsync over a tailnet)
//! moves that directory. The merge rules live here, once, so both transports
//! behave identically.
//!
//! Why the merge can be simple, and where it cannot:
//! - Brain nodes are content-addressed (`code:<ws-tag>:<file>:<hash of the
//!   chunk>`), so the same chunk has the same id on every machine. Merging is a
//!   set union, not a conflict resolution. Only `label` (it carries line
//!   numbers, so it differs between checkouts) and `access_cnt` move: label
//!   last-writer-wins, counter takes the maximum.
//! - Skills are markdown files. Identical → skip. One side changed → take it.
//!   Both changed → keep BOTH (`<name>.conflict-<host>.md`) instead of silently
//!   picking a winner.
//! - `MEMORY.md` is a set of entries separated by `§`: union with dedupe, then
//!   the same 2200-character cap the memory tool enforces (oldest fall off).
//! - Never exported, ever: `config.yaml` (API key), `.env`, caches, process
//!   output. The brain holds source code, so export runs only for workspaces on
//!   an explicit allowlist.

use crate::brain::db::{BrainDb, Node};
use crate::brain::indexer::blake3_simple;
use crate::config::Config;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const FORMAT_VERSION: u32 = 1;
/// Same delimiter the memory tool uses to store entries.
const MEMORY_DELIM: &str = "\n§\n";
/// Same cap as `tools/memory_tool.rs` (MAX_CHARS).
const MEMORY_MAX_CHARS: usize = 2200;
/// Files/dirs that must never appear in an export, whatever the config says.
const FORBIDDEN_NAMES: &[&str] = &[
    "config.yaml",
    ".env",
    "websurfx",
    "chrome-cdp",
    "staging",
    "tool_out",
    "audio",
    "proc",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub host: String,
    pub embedding_model: String,
    pub dim: usize,
    pub exported_at: String,
    pub workspaces: Vec<ManifestWorkspace>,
    pub nodes: usize,
    pub skills: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestWorkspace {
    /// Absolute path on the exporting machine.
    pub path: String,
    /// File inside `brain/` holding the nodes.
    pub file: String,
    /// Workspace tag used inside the node ids.
    pub tag: String,
    pub nodes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportedNode {
    id: String,
    label: String,
    content: String,
    kind: String,
    source: String,
    token_cost: usize,
    access_cnt: u64,
    workspace: String,
    /// 384 f32 as base64 — without it the other machine would have to re-embed
    /// everything (measured: 870 s for this repo).
    embedding: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExportReport {
    pub nodes: usize,
    pub workspaces: usize,
    pub skills: usize,
    pub memory: bool,
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub nodes_added: usize,
    pub nodes_updated: usize,
    pub nodes_foreign: usize,
    pub skills_copied: usize,
    pub skills_conflicts: usize,
    pub memory_entries: usize,
    pub memory_dropped: usize,
}

impl ImportReport {
    pub fn summary(&self) -> String {
        format!(
            "sync import: {} nodes added, {} updated, {} skipped (workspace not local), \
             {} skills copied, {} conflicts kept, memory entries {} ({} dropped by the cap)",
            self.nodes_added,
            self.nodes_updated,
            self.nodes_foreign,
            self.skills_copied,
            self.skills_conflicts,
            self.memory_entries,
            self.memory_dropped
        )
    }
}

impl ExportReport {
    pub fn summary(&self) -> String {
        format!(
            "sync export: {} nodes from {} workspace(s), {} skills, memory {}",
            self.nodes,
            self.workspaces,
            self.skills,
            if self.memory { "included" } else { "absent" }
        )
    }
}

/// Where the sync directory lives (config override, else `~/.rijo/sync`).
pub fn sync_dir(config: &Config) -> PathBuf {
    config.sync.dir.clone().unwrap_or_else(|| {
        config
            .data_dir
            .parent()
            .unwrap_or(Path::new("."))
            .join("sync")
    })
}

/// The machine name, for the sync manifest.
///
/// Order: what the system says, then the environment, then an honest fallback.
/// `/proc` is Linux only, `COMPUTERNAME` is the Windows name — before this,
/// every Windows host reported `unknown-host`, so two machines would have
/// carried the same name in the manifest and the conflict path of a merge
/// would have had nothing to distinguish them by.
fn hostname() -> String {
    if let Ok(h) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(h) = std::env::var(key) {
            let h = h.trim().to_string();
            if !h.is_empty() {
                return h;
            }
        }
    }
    // Nothing to read: name it after the workspace, so two such hosts differ.
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "host".to_string());
    format!("unknown-host@{cwd}")
}

fn ws_tag(workspace: &str) -> String {
    blake3_simple(workspace)[..8].to_string()
}

/// A manifest is an input file from a sync remote. Keep its host label inside
/// one harmless filename component before using it for conflict copies.
fn conflict_host_component(host: &str) -> String {
    let cleaned: String = host
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('.');
    if cleaned.is_empty() {
        "unknown-host".into()
    } else {
        cleaned.to_string()
    }
}

/// Workspace node filenames are generated as `<8-hex-tag>.jsonl`. Do not let
/// an imported manifest turn the brain-directory read into arbitrary file I/O.
fn valid_brain_filename(file: &str, tag: &str) -> bool {
    tag.len() == 8 && tag.chars().all(|c| c.is_ascii_hexdigit()) && file == format!("{tag}.jsonl")
}

fn canonical(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// Check the directory entry itself instead of following a sync-tree symlink.
/// Imported directories are data, not trusted filesystem paths.
fn checked_directory(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "refusing symlinked sync directory: {}",
            path.display()
        )),
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err(format!("sync path is not a directory: {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("cannot inspect {}: {e}", path.display())),
    }
}

fn checked_regular_file(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(format!("refusing symlinked sync file: {}", path.display()))
        }
        Ok(meta) if meta.is_file() => Ok(true),
        Ok(_) => Err(format!(
            "sync path is not a regular file: {}",
            path.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("cannot inspect {}: {e}", path.display())),
    }
}

fn encode_embedding(v: &[f32]) -> String {
    let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn decode_embedding(s: &str) -> Result<Vec<f32>, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .map_err(|e| format!("embedding not decodable: {e}"))?;
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Writes the portable directory: brain nodes per workspace (JSONL, one node per
/// line), skills, memory, manifest. Idempotent: the same state produces the same
/// files.
pub fn export(config: &Config, db: &BrainDb, dir: &Path) -> Result<ExportReport, String> {
    let brain_dir = dir.join("brain");
    std::fs::create_dir_all(&brain_dir).map_err(|e| e.to_string())?;
    // Regenerate instead of appending: a workspace removed from the allowlist
    // must disappear from the export too.
    if brain_dir.exists() {
        for entry in std::fs::read_dir(&brain_dir)
            .map_err(|e| e.to_string())?
            .flatten()
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }

    let mut report = ExportReport::default();
    let mut workspaces: Vec<ManifestWorkspace> = Vec::new();

    for ws in &config.sync.workspaces {
        let key = canonical(ws);
        let nodes = db.nodes_for_export(Some(&key))?;
        let tag = ws_tag(&key);
        let file = format!("{tag}.jsonl");
        let mut body = String::new();
        for (node, embedding) in &nodes {
            let exported = ExportedNode {
                id: node.id.clone(),
                label: node.label.clone(),
                content: node.content.clone(),
                kind: node.kind.clone(),
                source: node.source.clone(),
                token_cost: node.token_cost,
                access_cnt: node.access_cnt,
                workspace: node.workspace.clone(),
                embedding: embedding.as_ref().map(|v| encode_embedding(v)),
            };
            body.push_str(&serde_json::to_string(&exported).map_err(|e| e.to_string())?);
            body.push('\n');
        }
        std::fs::write(brain_dir.join(&file), body).map_err(|e| e.to_string())?;
        report.nodes += nodes.len();
        workspaces.push(ManifestWorkspace {
            path: key,
            file,
            tag,
            nodes: nodes.len(),
        });
    }
    report.workspaces = workspaces.len();

    // Skills: copy the tree file by file (never the excluded names).
    let skills_out = dir.join("skills");
    if skills_out.exists() {
        let _ = std::fs::remove_dir_all(&skills_out);
    }
    if config.skills_dir.exists() {
        report.skills = copy_tree(&config.skills_dir, &skills_out, &[])?;
    }

    // Memory: one file, same format the memory tool uses.
    let memory_in = config.data_dir.join("memories").join("MEMORY.md");
    let mut memory = false;
    if memory_in.exists() {
        let memories_out = dir.join("memories");
        std::fs::create_dir_all(&memories_out).map_err(|e| e.to_string())?;
        std::fs::copy(&memory_in, memories_out.join("MEMORY.md")).map_err(|e| e.to_string())?;
        memory = true;
    }
    report.memory = memory;

    let manifest = Manifest {
        version: FORMAT_VERSION,
        host: hostname(),
        embedding_model: config.memory.embedding_model.clone(),
        dim: config.memory.embedding_dim,
        exported_at: chrono::Utc::now().to_rfc3339(),
        workspaces,
        nodes: report.nodes,
        skills: report.skills,
    };
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    Ok(report)
}

fn copy_tree(src: &Path, dst: &Path, _relative: &[&str]) -> Result<usize, String> {
    if !checked_directory(src)? {
        return Ok(0);
    }
    let mut copied = 0;
    let entries = std::fs::read_dir(src).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if FORBIDDEN_NAMES.contains(&name.as_str()) || crate::trust::is_env_file(Path::new(&name)) {
            continue;
        }
        let file_type = entry.file_type().map_err(|e| e.to_string())?;
        if file_type.is_symlink() {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if file_type.is_dir() {
            std::fs::create_dir_all(&to).map_err(|e| e.to_string())?;
            copied += copy_tree(&from, &to, _relative)?;
        } else if file_type.is_file() {
            std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
            std::fs::copy(&from, &to).map_err(|e| e.to_string())?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// Reads a sync directory and merges it into this machine's state.
///
/// Refuses to import when the embedding model or dimension differs: foreign
/// vectors are worse than none, because they silently degrade every search.
pub fn import(config: &Config, db: &BrainDb, dir: &Path) -> Result<ImportReport, String> {
    let manifest_path = dir.join("manifest.json");
    if !checked_regular_file(&manifest_path)? {
        return Err(format!("no manifest at {}", manifest_path.display()));
    }
    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("no manifest at {}: {e}", manifest_path.display()))?;
    let manifest: Manifest =
        serde_json::from_str(&raw).map_err(|e| format!("manifest unreadable: {e}"))?;

    if manifest.version != FORMAT_VERSION {
        return Err(format!(
            "sync format version {} is not supported (this build speaks {})",
            manifest.version, FORMAT_VERSION
        ));
    }
    if manifest.dim != config.memory.embedding_dim {
        return Err(format!(
            "embedding dimension mismatch: export has {}, this machine uses {} — pointers would be meaningless. \
             Align memory.embedding_dim on both machines, then export again.",
            manifest.dim, config.memory.embedding_dim
        ));
    }
    // Model name mismatch is a hard stop too: same dimension, different model is
    // still incomparable (different vector space).
    if !manifest.embedding_model.is_empty()
        && !config.memory.embedding_model.is_empty()
        && manifest.embedding_model != config.memory.embedding_model
    {
        return Err(format!(
            "embedding model mismatch: export used '{}', this machine uses '{}'",
            manifest.embedding_model, config.memory.embedding_model
        ));
    }

    let local_workspaces: Vec<String> = config
        .sync
        .workspaces
        .iter()
        .map(|p| canonical(p))
        .collect();
    let mut report = ImportReport::default();
    let remote_host = conflict_host_component(&manifest.host);
    let brain_dir = dir.join("brain");

    for ws in &manifest.workspaces {
        if !valid_brain_filename(&ws.file, &ws.tag) {
            return Err(format!(
                "invalid workspace brain filename {:?} for tag {:?}",
                ws.file, ws.tag
            ));
        }
        if !checked_directory(&brain_dir)? {
            return Err(format!("missing brain directory: {}", brain_dir.display()));
        }
        let brain_file = brain_dir.join(&ws.file);
        if !checked_regular_file(&brain_file)? {
            return Err(format!("{} unreadable: file is missing", ws.file));
        }
        // Where do these nodes belong on this machine? Exact path first, then an
        // explicit mapping, else skip (and say so) — importing a foreign
        // workspace would produce pointers to files that do not exist here.
        let local_target = if local_workspaces.iter().any(|p| p == &ws.path) {
            Some((ws.path.clone(), ws.tag.clone()))
        } else if let Some(mapped) = config.sync.workspace_map.get(&ws.path) {
            let mapped_key = canonical(mapped);
            Some((mapped_key.clone(), ws_tag(&mapped_key)))
        } else {
            None
        };
        let Some((target_ws, target_tag)) = local_target else {
            report.nodes_foreign += count_lines(&brain_file);
            continue;
        };

        let body = std::fs::read_to_string(&brain_file)
            .map_err(|e| format!("{} unreadable: {e}", ws.file))?;
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            let mut node: ExportedNode =
                serde_json::from_str(line).map_err(|e| format!("node line unreadable: {e}"))?;
            // Rewrite the tag when the workspace lives elsewhere (same content,
            // other path) so the local indexer does not create a duplicate node
            // for the same chunk.
            if target_tag != ws.tag {
                node.id = rewrite_tag(&node.id, &ws.tag, &target_tag);
            }
            node.workspace = target_ws.clone();
            let embedding = match &node.embedding {
                Some(b64) => Some(decode_embedding(b64)?),
                None => None,
            };
            let incoming = Node {
                id: node.id.clone(),
                label: node.label.clone(),
                content: node.content.clone(),
                kind: node.kind.clone(),
                source: node.source.clone(),
                workspace: node.workspace.clone(),
                token_cost: node.token_cost,
                created_at: 0,
                updated_at: 0,
                access_cnt: node.access_cnt,
                relevance: 1.0,
            };
            match db.get_node(&node.id)? {
                Some(local) => {
                    // Merge: label last-writer-wins, counter max, everything else
                    // comes from the content-addressed identity anyway.
                    let merged = Node {
                        id: local.id.clone(),
                        label: node.label.clone(),
                        content: local.content.clone(),
                        kind: local.kind.clone(),
                        source: local.source.clone(),
                        workspace: local.workspace.clone(),
                        token_cost: local.token_cost,
                        created_at: local.created_at,
                        updated_at: local.updated_at,
                        access_cnt: local.access_cnt.max(node.access_cnt),
                        relevance: local.relevance,
                    };
                    db.upsert_node(&merged, None)?;
                    report.nodes_updated += 1;
                }
                None => {
                    db.upsert_node(&incoming, embedding.as_deref())?;
                    report.nodes_added += 1;
                }
            }
        }
    }

    // Skills: copy what is missing, report what collides.
    let skills_in = dir.join("skills");
    if checked_directory(&skills_in)? {
        std::fs::create_dir_all(&config.skills_dir).map_err(|e| e.to_string())?;
        let (copied, conflicts) = merge_tree(
            &skills_in,
            &config.skills_dir,
            &config.skills_dir,
            &remote_host,
        )?;
        report.skills_copied = copied;
        report.skills_conflicts = conflicts;
    }

    // Memory: union of entries, dedupe, cap.
    let memories_dir = dir.join("memories");
    let memory_in = memories_dir.join("MEMORY.md");
    if checked_directory(&memories_dir)? && checked_regular_file(&memory_in)? {
        let remote = std::fs::read_to_string(&memory_in).map_err(|e| e.to_string())?;
        let local_path = config.data_dir.join("memories").join("MEMORY.md");
        let local = std::fs::read_to_string(&local_path).unwrap_or_default();
        let (merged, total, dropped) = merge_memory(&local, &remote, MEMORY_MAX_CHARS);
        if let Some(parent) = local_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&local_path, merged).map_err(|e| e.to_string())?;
        report.memory_entries = total;
        report.memory_dropped = dropped;
    }

    Ok(report)
}

fn count_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

fn rewrite_tag(id: &str, from: &str, to: &str) -> String {
    // id layout: code:<tag>:<path>:<hash>
    id.replacen(from, to, 1)
}

fn merge_tree(
    src: &Path,
    dst: &Path,
    writable_root: &Path,
    remote_host: &str,
) -> Result<(usize, usize), String> {
    if !crate::trust::is_in_workspace(dst, writable_root) {
        return Err(format!(
            "refusing to merge skills outside the writable root: {}",
            dst.display()
        ));
    }
    let mut copied = 0;
    let mut conflicts = 0;
    for entry in std::fs::read_dir(src).map_err(|e| e.to_string())?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if FORBIDDEN_NAMES.contains(&name.as_str()) || crate::trust::is_env_file(Path::new(&name)) {
            continue;
        }
        let file_type = entry.file_type().map_err(|e| e.to_string())?;
        if file_type.is_symlink() {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if !crate::trust::is_in_workspace(&to, writable_root) {
            return Err(format!(
                "refusing to merge a skill path outside the writable root: {}",
                to.display()
            ));
        }
        if std::fs::symlink_metadata(&to)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(format!(
                "refusing to merge through skill symlink: {}",
                to.display()
            ));
        }
        if file_type.is_dir() {
            std::fs::create_dir_all(&to).map_err(|e| e.to_string())?;
            let (c, k) = merge_tree(&from, &to, writable_root, remote_host)?;
            copied += c;
            conflicts += k;
            continue;
        }
        std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
        let remote = std::fs::read_to_string(&from).map_err(|e| e.to_string())?;
        match std::fs::read_to_string(&to) {
            Ok(local) if local == remote => {}
            Ok(_) => {
                // Both sides changed it. Guessing would lose one of them.
                let stem = Path::new(&name)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| name.clone());
                let ext = Path::new(&name)
                    .extension()
                    .map(|s| format!(".{}", s.to_string_lossy()))
                    .unwrap_or_default();
                let conflict = dst.join(format!("{stem}.conflict-{remote_host}{ext}"));
                std::fs::write(&conflict, remote).map_err(|e| e.to_string())?;
                conflicts += 1;
            }
            Err(_) => {
                std::fs::write(&to, remote).map_err(|e| e.to_string())?;
                copied += 1;
            }
        }
    }
    Ok((copied, conflicts))
}

/// Union of `§`-separated entries: local order first, then unseen remote ones.
/// Over the cap the oldest entries fall off (front of the list), like the memory
/// tool behaves.
fn merge_memory(local: &str, remote: &str, max_chars: usize) -> (String, usize, usize) {
    let split = |s: &str| -> Vec<String> {
        s.split(MEMORY_DELIM)
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .collect()
    };
    let mut entries = split(local);
    for entry in split(remote) {
        if !entries.iter().any(|e| e == &entry) {
            entries.push(entry);
        }
    }
    let mut dropped = 0;
    loop {
        let joined = entries.join(MEMORY_DELIM);
        if joined.chars().count() <= max_chars || entries.is_empty() {
            return (joined, entries.len(), dropped);
        }
        entries.remove(0);
        dropped += 1;
    }
}

/// Workspace allowlist check used by the CLI before exporting anything: the
/// brain holds source code, so an empty allowlist must not mean "everything".
pub fn export_targets(config: &Config) -> Vec<PathBuf> {
    config
        .sync
        .workspaces
        .iter()
        .filter(|p| p.exists())
        .cloned()
        .collect()
}

/// Marker the manifest of a locally generated (not yet pushed) directory gets,
/// used by `--sync-status`.
pub fn describe(config: &Config) -> String {
    let dir = sync_dir(config);
    let mut lines = vec![
        format!("sync dir:  {}", dir.display()),
        format!("transport:  {}", config.sync.transport),
        format!(
            "repo:       {}",
            config
                .sync
                .repo
                .clone()
                .unwrap_or_else(|| "(not configured)".into())
        ),
        format!("enabled:    {}", config.sync.enabled),
    ];
    for ws in export_targets(config) {
        let key = canonical(&ws);
        let tag = ws_tag(&key);
        lines.push(format!("workspace:  {key} (tag {tag})"));
    }
    lines.join("\n")
}

/// Node/workspace counts of an existing sync directory (for `--sync-status`).
pub fn inspect(dir: &Path) -> Result<BTreeMap<String, usize>, String> {
    let mut out = BTreeMap::new();
    let manifest = dir.join("manifest.json");
    if !manifest.exists() {
        return Ok(out);
    }
    let parsed: Manifest =
        serde_json::from_str(&std::fs::read_to_string(&manifest).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    out.insert("nodes".into(), parsed.nodes);
    out.insert("workspaces".into(), parsed.workspaces.len());
    out.insert("skills".into(), parsed.skills);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::db::{BrainDb, SearchQuery};

    struct Fixture {
        #[allow(dead_code)] // kept alive for the test: dropping it deletes the temp dir
        tmp: tempfile::TempDir,
        config: Config,
        db: BrainDb,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut config = Config::default();
        config.data_dir = root.join("data");
        config.skills_dir = root.join("skills");
        config.workspace = root.join("ws");
        std::fs::create_dir_all(&config.data_dir).unwrap();
        std::fs::create_dir_all(&config.skills_dir).unwrap();
        std::fs::create_dir_all(&config.workspace).unwrap();
        config.sync.dir = Some(root.join("sync"));
        config.sync.workspaces = vec![config.workspace.clone()];
        config.memory.embedding_dim = 3;
        config.memory.embedding_model = "test-model".into();
        let db = BrainDb::open(&root.join("brain.db"), 3).unwrap();
        Fixture { tmp, config, db }
    }

    #[test]
    fn imported_host_cannot_escape_skill_conflict_path() {
        let f = fixture();
        let local_skill = f.config.skills_dir.join("demo");
        // The traversal prefix is a directory so a vulnerable `dst.join(..)`
        // can actually walk through it on the filesystem.
        std::fs::create_dir_all(local_skill.join("SKILL.conflict-..")).unwrap();
        std::fs::write(local_skill.join("SKILL.md"), "local skill\n").unwrap();

        let sync = sync_dir(&f.config);
        let remote_skill = sync.join("skills").join("demo");
        std::fs::create_dir_all(&remote_skill).unwrap();
        std::fs::write(remote_skill.join("SKILL.md"), "remote skill\n").unwrap();
        let manifest = Manifest {
            version: FORMAT_VERSION,
            host: "../../../../escape".into(),
            embedding_model: f.config.memory.embedding_model.clone(),
            dim: f.config.memory.embedding_dim,
            exported_at: chrono::Utc::now().to_rfc3339(),
            workspaces: Vec::new(),
            nodes: 0,
            skills: 1,
        };
        std::fs::write(
            sync.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let escaped = f.tmp.path().join("escape.md");
        let _ = std::fs::remove_file(&escaped);
        let result = import(&f.config, &f.db, &sync);

        assert!(
            result.is_ok(),
            "safe conflict import should complete: {result:?}"
        );
        assert!(
            !escaped.exists(),
            "manifest.host escaped skills_dir and wrote {}",
            escaped.display()
        );
        let conflicts: Vec<_> = std::fs::read_dir(&local_skill)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains("conflict")
            })
            .collect();
        assert_eq!(
            conflicts.len(),
            1,
            "conflict copy should remain in skill dir"
        );
    }

    #[test]
    fn imported_manifest_cannot_read_a_brain_file_outside_sync_dir() {
        let f = fixture();
        let sync = sync_dir(&f.config);
        let brain = sync.join("brain");
        std::fs::create_dir_all(&brain).unwrap();
        let workspace = canonical(&f.config.workspace);
        let tag = ws_tag(&workspace);
        let injected_id = format!("code:{tag}:outside.rs:deadbeef");
        let outside = f.tmp.path().join("outside.jsonl");
        let node = ExportedNode {
            id: injected_id.clone(),
            label: "outside file".into(),
            content: "must not be imported".into(),
            kind: "code".into(),
            source: "outside.rs".into(),
            token_cost: 5,
            access_cnt: 0,
            workspace: workspace.clone(),
            embedding: None,
        };
        std::fs::write(
            &outside,
            format!("{}\n", serde_json::to_string(&node).unwrap()),
        )
        .unwrap();
        let manifest = Manifest {
            version: FORMAT_VERSION,
            host: "remote".into(),
            embedding_model: f.config.memory.embedding_model.clone(),
            dim: f.config.memory.embedding_dim,
            exported_at: chrono::Utc::now().to_rfc3339(),
            workspaces: vec![ManifestWorkspace {
                path: workspace,
                file: "../../outside.jsonl".into(),
                tag,
                nodes: 1,
            }],
            nodes: 1,
            skills: 0,
        };
        std::fs::write(
            sync.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let result = import(&f.config, &f.db, &sync);
        assert!(
            result.is_err(),
            "path traversal in manifest was accepted: {result:?}"
        );
        assert!(f.db.get_node(&injected_id).unwrap().is_none());
    }

    #[test]
    fn import_does_not_follow_a_symlinked_skills_root() {
        let f = fixture();
        let sync = sync_dir(&f.config);
        let external = f.tmp.path().join("external-skills");
        let skill = external.join("escaped-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "outside skill\n").unwrap();
        let skills_link = sync.join("skills");
        std::fs::create_dir_all(&sync).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&external, &skills_link).unwrap();
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd.exe")
                .args([
                    "/D",
                    "/C",
                    "mklink",
                    "/J",
                    skills_link.to_str().unwrap(),
                    external.to_str().unwrap(),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("cmd.exe");
            assert!(
                status.success(),
                "could not create skills junction for test"
            );
        }
        let manifest = Manifest {
            version: FORMAT_VERSION,
            host: "remote".into(),
            embedding_model: f.config.memory.embedding_model.clone(),
            dim: f.config.memory.embedding_dim,
            exported_at: chrono::Utc::now().to_rfc3339(),
            workspaces: Vec::new(),
            nodes: 0,
            skills: 1,
        };
        std::fs::write(
            sync.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let result = import(&f.config, &f.db, &sync);
        assert!(
            result.is_err(),
            "import followed symlinked skills root: {result:?}"
        );
        assert!(!f.config.skills_dir.join("escaped-skill").exists());
    }

    fn seed(db: &BrainDb, workspace: &std::path::Path, name: &str, embedding: [f32; 3]) {
        let key = canonical(workspace);
        let node = Node::new(
            format!("code:{}:{}:deadbeef", ws_tag(&key), name),
            format!("{name} — fn thing"),
            format!("content of {name}"),
            "code",
        )
        .with_source(name)
        .with_workspace(&key);
        db.upsert_node(&node, Some(&embedding)).unwrap();
    }

    #[test]
    fn export_then_import_into_a_fresh_machine_is_idempotent() {
        let a = fixture();
        seed(&a.db, &a.config.workspace.clone(), "a.rs", [1.0, 0.0, 0.0]);
        seed(&a.db, &a.config.workspace.clone(), "b.rs", [0.0, 1.0, 0.0]);
        let dir = sync_dir(&a.config);
        let export = export(&a.config, &a.db, &dir).unwrap();
        assert_eq!(export.nodes, 2, "{export:?}");

        // Second machine: same layout, empty brain.
        let b = fixture();
        std::fs::create_dir_all(b.config.workspace.clone()).unwrap();
        // Point B at the same workspace path so the allowlist matches.
        let mut b_config = b.config.clone();
        b_config.sync.workspaces = vec![a.config.workspace.clone()];
        let first = import(&b_config, &b.db, &dir).unwrap();
        assert_eq!(first.nodes_added, 2, "{}", first.summary());
        assert_eq!(b.db.count_nodes(None).unwrap(), 2);

        // Re-import must not duplicate anything.
        let second = import(&b_config, &b.db, &dir).unwrap();
        assert_eq!(second.nodes_added, 0, "{}", second.summary());
        assert_eq!(
            b.db.count_nodes(None).unwrap(),
            2,
            "import duplicated nodes"
        );
    }

    #[test]
    fn imported_embeddings_are_usable_for_search() {
        let a = fixture();
        seed(
            &a.db,
            &a.config.workspace.clone(),
            "hit.rs",
            [1.0, 0.0, 0.0],
        );
        let dir = sync_dir(&a.config);
        export(&a.config, &a.db, &dir).unwrap();

        let b = fixture();
        let mut b_config = b.config.clone();
        b_config.sync.workspaces = vec![a.config.workspace.clone()];
        import(&b_config, &b.db, &dir).unwrap();

        // Import a second time: the merge branch runs for every node now, and it
        // used to wipe the embeddings (found by a real run, not by the tests).
        import(&b_config, &b.db, &dir).unwrap();

        // The vector must have survived the base64 round trip.
        let res =
            b.db.search(&SearchQuery {
                text: "unused",
                embedding: Some(&[1.0, 0.0, 0.0]),
                kind: None,
                workspace: Some(&canonical(&a.config.workspace)),
                top_k: 3,
                min_score: 0.9,
            })
            .unwrap();
        assert_eq!(
            res.hits.len(),
            1,
            "imported embedding is not searchable: {:?}",
            res.hits.len()
        );
        assert_eq!(res.hits[0].node.source, "hit.rs");
    }

    #[test]
    fn embedding_dimension_mismatch_is_refused() {
        let a = fixture();
        seed(&a.db, &a.config.workspace.clone(), "a.rs", [1.0, 0.0, 0.0]);
        let dir = sync_dir(&a.config);
        export(&a.config, &a.db, &dir).unwrap();

        let b = fixture();
        let mut b_config = b.config.clone();
        b_config.memory.embedding_dim = 384; // other model on the other machine
        let err = import(&b_config, &b.db, &dir).unwrap_err();
        assert!(err.contains("dimension mismatch"), "{err}");
    }

    #[test]
    fn embedding_model_mismatch_is_refused() {
        let a = fixture();
        seed(&a.db, &a.config.workspace.clone(), "a.rs", [1.0, 0.0, 0.0]);
        let dir = sync_dir(&a.config);
        export(&a.config, &a.db, &dir).unwrap();

        let b = fixture();
        let mut b_config = b.config.clone();
        b_config.memory.embedding_model = "other-model".into();
        let err = import(&b_config, &b.db, &dir).unwrap_err();
        assert!(err.contains("model mismatch"), "{err}");
    }

    #[test]
    fn foreign_workspace_is_skipped_instead_of_pointing_nowhere() {
        let a = fixture();
        seed(&a.db, &a.config.workspace.clone(), "a.rs", [1.0, 0.0, 0.0]);
        let dir = sync_dir(&a.config);
        export(&a.config, &a.db, &dir).unwrap();

        // B does not have that workspace in its allowlist.
        let b = fixture();
        let report = import(&b.config, &b.db, &dir).unwrap();
        assert_eq!(report.nodes_added, 0, "{}", report.summary());
        assert_eq!(report.nodes_foreign, 1, "{}", report.summary());
        assert_eq!(b.db.count_nodes(None).unwrap(), 0);
    }

    #[test]
    fn workspace_map_rebases_the_nodes_onto_the_local_path() {
        let a = fixture();
        seed(&a.db, &a.config.workspace.clone(), "a.rs", [1.0, 0.0, 0.0]);
        let dir = sync_dir(&a.config);
        export(&a.config, &a.db, &dir).unwrap();

        // B checks the same project out somewhere else.
        let b = fixture();
        let mut b_config = b.config.clone();
        b_config.sync.workspaces = vec![b.config.workspace.clone()];
        b_config
            .sync
            .workspace_map
            .insert(canonical(&a.config.workspace), b.config.workspace.clone());
        let report = import(&b_config, &b.db, &dir).unwrap();
        assert_eq!(report.nodes_added, 1, "{}", report.summary());

        // The node must now belong to B's path and be findable under it.
        let res =
            b.db.search(&SearchQuery {
                text: "content",
                embedding: Some(&[1.0, 0.0, 0.0]),
                kind: None,
                workspace: Some(&canonical(&b.config.workspace)),
                top_k: 3,
                min_score: 0.9,
            })
            .unwrap();
        assert_eq!(res.hits.len(), 1, "node was not rebased: {report:?}");
        let id = res.hits[0].node.id.clone();
        assert!(
            id.contains(&ws_tag(&canonical(&b.config.workspace))),
            "tag not rewritten: {id}"
        );
    }

    #[test]
    fn conflicting_skill_keeps_both_versions() {
        let a = fixture();
        let skill_a = a.config.skills_dir.join("demo");
        std::fs::create_dir_all(&skill_a).unwrap();
        std::fs::write(skill_a.join("SKILL.md"), "version A").unwrap();
        let dir = sync_dir(&a.config);
        export(&a.config, &a.db, &dir).unwrap();

        // B has its own, different version of the same skill.
        let b = fixture();
        let skill_b = b.config.skills_dir.join("demo");
        std::fs::create_dir_all(&skill_b).unwrap();
        std::fs::write(skill_b.join("SKILL.md"), "version B").unwrap();
        let report = import(&b.config, &b.db, &dir).unwrap();

        assert_eq!(report.skills_conflicts, 1, "{}", report.summary());
        assert_eq!(
            std::fs::read_to_string(skill_b.join("SKILL.md")).unwrap(),
            "version B"
        );
        let entries: Vec<String> = std::fs::read_dir(&skill_b)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            entries.iter().any(|e| e.contains(".conflict-")),
            "the other version must be kept, not dropped: {entries:?}"
        );
    }

    #[test]
    fn missing_skill_is_copied_and_identical_one_is_skipped() {
        let a = fixture();
        let skill = a.config.skills_dir.join("only-here");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "content").unwrap();
        let dir = sync_dir(&a.config);
        export(&a.config, &a.db, &dir).unwrap();

        let b = fixture();
        let report = import(&b.config, &b.db, &dir).unwrap();
        assert_eq!(report.skills_copied, 1, "{}", report.summary());
        assert!(b
            .config
            .skills_dir
            .join("only-here")
            .join("SKILL.md")
            .exists());

        let again = import(&b.config, &b.db, &dir).unwrap();
        assert_eq!(again.skills_copied, 0, "identical skill was copied twice");
        assert_eq!(again.skills_conflicts, 0);
    }

    #[test]
    fn memory_is_unioned_deduped_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let (merged, total, dropped) = merge_memory("a\n§\nb", "b\n§\nc", 10_000);
        assert_eq!(merged, "a\n§\nb\n§\nc");
        assert_eq!(total, 3);
        assert_eq!(dropped, 0);

        // Cap: the oldest entries fall off.
        let (capped, total, dropped) = merge_memory("first-long-entry\n§\nsecond", "third", 20);
        assert!(
            !capped.starts_with("first-long-entry"),
            "oldest entry should fall off: {capped}"
        );
        assert_eq!(dropped, 1, "drop count: {dropped}");
        assert!(total <= 3);
        drop(dir);
    }

    #[test]
    fn secrets_and_caches_are_never_exported() {
        let a = fixture();
        // Everything that must never leave the machine.
        std::fs::write(
            a.config.data_dir.join("config.yaml"),
            "llm:\n  api_key: SECRET",
        )
        .unwrap();
        std::fs::write(
            a.config.data_dir.join(".env"),
            "WEBSURFX_URL=http://localhost:8080",
        )
        .unwrap();
        std::fs::create_dir_all(a.config.data_dir.join("websurfx")).unwrap();
        std::fs::write(a.config.data_dir.join("websurfx").join("binary"), "1.1 GB").unwrap();
        std::fs::create_dir_all(a.config.skills_dir.join("websurfx")).unwrap();
        std::fs::write(a.config.skills_dir.join("websurfx").join("x"), "no").unwrap();
        std::fs::write(
            a.config.skills_dir.join(".env.local"),
            "LOCAL_SECRET=must-not-sync",
        )
        .unwrap();
        seed(&a.db, &a.config.workspace.clone(), "ok.rs", [1.0, 0.0, 0.0]);

        let dir = sync_dir(&a.config);
        export(&a.config, &a.db, &dir).unwrap();

        let mut found: Vec<String> = Vec::new();
        let mut stack = vec![dir.clone()];
        while let Some(current) = stack.pop() {
            for entry in std::fs::read_dir(&current).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                found.push(path.file_name().unwrap().to_string_lossy().to_string());
            }
        }
        for forbidden in FORBIDDEN_NAMES {
            assert!(
                !found.iter().any(|f| f == forbidden),
                "{forbidden} made it into the export: {found:?}"
            );
        }
        assert!(
            !found.iter().any(|f| f == ".env.local"),
            "an env variant made it into the export: {found:?}"
        );
        // …but the legitimate node is there.
        let brain = std::fs::read_to_string(
            dir.join("brain")
                .join(format!("{}.jsonl", ws_tag(&canonical(&a.config.workspace)))),
        )
        .unwrap();
        assert!(brain.contains("ok.rs"), "the node itself must be exported");
        assert!(!brain.contains("SECRET"), "secret leaked into a node line");
    }

    /// The manifest names the host, and a merge distinguishes two machines by
    /// it. `/proc/sys/kernel/hostname` does not exist on Windows, so before
    /// this every Windows host reported the same fallback and the conflict path
    /// had nothing to tell them apart.
    #[test]
    fn the_hostname_is_not_the_bare_fallback() {
        let h = hostname();
        assert!(!h.is_empty());
        assert_ne!(
            h, "unknown-host",
            "hostname() fell back - the platform probe did not work"
        );
        assert!(
            !h.contains('/') && !h.contains('\\'),
            "a host name must be a single component, got {h}"
        );

        if cfg!(windows) {
            // On Windows /proc is absent, so the environment path must answer.
            assert_eq!(
                h,
                std::env::var("COMPUTERNAME").unwrap().trim(),
                "COMPUTERNAME must be the answer where /proc does not exist"
            );
        }
    }
}
