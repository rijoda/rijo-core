//! Codebase Indexer — chunks, embeds and stores code in the brain.
//!
//! Flow:
//! 1. Scan the workspace for relevant source files
//! 2. Split files into sensible chunks (per function/class/100 lines)
//! 3. Embed the chunk + store it in brain.db
//! 4. Remove old chunks for deleted files
//!
//! Result: the LLM can use "brain_query" instead of read_file — no re-reading
//! of known files.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::db::{BrainDb, Node};
use super::embed::Embedder;

/// What the indexer needs from an embedder. Exists so the incremental logic can
/// be tested with a deterministic stub instead of the real ONNX model (a test
/// that downloads a model is not a test, it is a network dependency).
pub trait ChunkEmbedder: Send + Sync {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String>;
}

impl ChunkEmbedder for Embedder {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        Embedder::embed_batch(self, texts)
    }
}

/// Language-specific patterns for relevant files and chunk boundaries
struct LangConfig {
    exts: &'static [&'static str],
    /// Patterns at which a new chunk begins (Rust: fn, impl, pub ...)
    chunk_patterns: &'static [&'static str],
    /// Maximum chunk size in lines (for languages without patterns)
    max_chunk_lines: usize,
}

const LANG_CONFIGS: &[LangConfig] = &[
    LangConfig {
        exts: &["rs"],
        chunk_patterns: &[
            "pub fn ",
            "fn ",
            "pub struct",
            "struct ",
            "pub enum",
            "enum ",
            "pub trait",
            "trait ",
            "impl ",
            "pub mod",
            "mod ",
            "pub const",
            "pub type",
            "unsafe fn",
            "async fn",
            "pub async fn",
            "#[test]",
            "#[cfg(test)]",
            "macro_rules!",
        ],
        max_chunk_lines: 80,
    },
    LangConfig {
        exts: &["py"],
        chunk_patterns: &["class ", "def ", "async def ", "@", "import ", "from "],
        max_chunk_lines: 100,
    },
    LangConfig {
        exts: &["js", "jsx", "ts", "tsx", "mjs", "cjs"],
        chunk_patterns: &[
            "export ",
            "function ",
            "class ",
            "const ",
            "import ",
            "interface ",
            "type ",
            "enum ",
        ],
        max_chunk_lines: 80,
    },
    LangConfig {
        exts: &["go"],
        chunk_patterns: &["func ", "type ", "struct ", "interface ", "package "],
        max_chunk_lines: 80,
    },
    LangConfig {
        exts: &["c", "h", "cpp", "hpp", "cc", "cxx"],
        chunk_patterns: &[
            "class ",
            "struct ",
            "enum ",
            "namespace ",
            "template ",
            "typedef ",
            "using namespace",
        ],
        max_chunk_lines: 80,
    },
    LangConfig {
        exts: &["zig"],
        chunk_patterns: &["fn ", "pub fn ", "const ", "var "],
        max_chunk_lines: 80,
    },
    LangConfig {
        // Fallback: all other text files
        exts: &[
            "md", "txt", "toml", "yaml", "yml", "json", "html", "css", "scss",
        ],
        chunk_patterns: &["# ", "## ", "### ", "---"],
        max_chunk_lines: 50,
    },
];

fn get_lang_config(path: &Path) -> Option<&'static LangConfig> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    LANG_CONFIGS
        .iter()
        .find(|cfg| cfg.exts.contains(&ext.as_str()))
}

fn should_ignore(entry: &std::fs::DirEntry) -> bool {
    let name = entry.file_name();
    let s = name.to_string_lossy();
    // Always ignore
    if s.starts_with('.')
        || s == "node_modules"
        || s == "target"
        || s == "build"
        || s == "dist"
        || s == ".git"
        || s == "__pycache__"
        || s == ".venv"
        || s == "venv"
        || s == "vendor"
        || s == ".rijo"
        || s == ".hermes"
        || s == "Cargo.lock"
        || s == "package-lock.json"
        || s == "pnpm-lock.yaml"
        || s == "uv.lock"
    {
        return true;
    }
    // Only directories and regular files
    entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(false)
}

/// Collects all relevant files in the workspace (recursively).
pub fn collect_files(workspace: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !workspace.is_dir() {
        return files;
    }
    visit_dirs(workspace, &mut files);
    files
}

fn visit_dirs(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if should_ignore(&entry) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            visit_dirs(&path, files);
        } else if path.is_file() && get_lang_config(&path).is_some() {
            files.push(path);
        }
    }
}

/// Chunks a file into sensible sections.
/// Returns Vec<(label, content)>.
fn chunk_file(content: &str, path: &Path, config: &LangConfig) -> Vec<(String, String)> {
    let lines: Vec<&str> = content.lines().collect();
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default()
        .to_string();

    if lines.len() <= config.max_chunk_lines {
        // Small file = 1 chunk
        let label = if let Some(first) = lines.first() {
            format!(
                "{} — {}",
                filename,
                first.trim().chars().take(60).collect::<String>()
            )
        } else {
            filename.clone()
        };
        return vec![(label, content.to_string())];
    }

    // Large file: split at chunk_patterns
    let mut chunks: Vec<(String, Vec<(usize, &str)>)> = Vec::new();
    let mut current_label = filename.clone();
    let mut current_lines: Vec<(usize, &str)> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let is_chunk_start = config.chunk_patterns.iter().any(|p| trimmed.starts_with(p));

        if is_chunk_start && !current_lines.is_empty() && current_lines.len() >= 3 {
            chunks.push((current_label.clone(), current_lines));
            current_label = format!(
                "{}:{} — {}",
                filename,
                i + 1,
                trimmed.chars().take(60).collect::<String>()
            );
            current_lines = Vec::new();
        }

        current_lines.push((i + 1, line));

        // Hard cap: max_chunk_lines lines per chunk
        if current_lines.len() >= config.max_chunk_lines {
            chunks.push((current_label.clone(), current_lines));
            current_label = format!("{}:{}", filename, i + 2);
            current_lines = Vec::new();
        }
    }

    if !current_lines.is_empty() {
        chunks.push((current_label, current_lines));
    }

    chunks
        .into_iter()
        .map(|(label, lines)| {
            let first_lineno = lines.first().map(|(no, _)| *no).unwrap_or(1);
            let content = lines
                .iter()
                .map(|(_, line)| *line)
                .collect::<Vec<_>>()
                .join("\n");
            let full_label = if first_lineno == 1 {
                label
            } else {
                format!("{} (line {})", label, first_lineno)
            };
            (full_label, content)
        })
        .collect()
}

/// Indexes the workspace into brain.db — incrementally.
///
/// Per file a cheap `stat` is compared against `indexed_files` (mtime + size).
/// Unchanged files are not read, not chunked and not embedded at all; a warm
/// start is a stat pass. Changed files have their old nodes removed before the
/// new ones are written, so a deleted function cannot survive as a ghost hit.
/// Files that no longer exist lose their nodes too.
pub fn index_workspace(
    db: &BrainDb,
    embedder: &dyn ChunkEmbedder,
    workspace: &Path,
) -> Result<IndexSummary, String> {
    let files = collect_files(workspace);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let _ = now;
    // Every node carries its workspace root, and the node id is namespaced with
    // it: `src/main.rs` in two projects used to collide on the same id.
    let ws_key = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
        .to_string_lossy()
        .to_string();
    let ws_tag = blake3_simple(&ws_key)[..8].to_string();

    // Symbol nodes were added after the first index generation: one marker per
    // workspace forces a single full re-index so existing workspaces get them.
    let symbol_key = format!("symbols_v2:{ws_tag}");
    let symbols_ready = db.kv_cache_get(&symbol_key).is_some();

    // One clean sweep: nodes without a workspace (pre-column leftovers) are
    // invisible to the scoped search and would only waste space.
    let dropped_unattributed = db.drop_unattributed_nodes().unwrap_or(0);

    let mut dirty: Vec<(
        String,
        i64,
        i64,
        Vec<(String, String)>,
        Vec<crate::brain::outline::Symbol>,
    )> = Vec::new();
    let mut unchanged = 0u64;
    let mut errors = 0u64;

    for filepath in &files {
        let rel_path = filepath.strip_prefix(workspace).unwrap_or(filepath);
        let rel_str = rel_path.to_string_lossy().to_string();

        let meta = match std::fs::metadata(filepath) {
            Ok(m) => m,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let size = meta.len() as i64;

        match db.file_state(&ws_key, &rel_str) {
            Ok(Some((known_mtime, known_size, _)))
                if known_mtime == mtime && known_size == size && symbols_ready =>
            {
                // Nothing changed: not read, not embedded.
                unchanged += 1;
                continue;
            }
            _ => {}
        }

        let content = match std::fs::read_to_string(filepath) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[brain] cannot read {rel_str}: {e}");
                errors += 1;
                continue;
            }
        };
        let Some(config) = get_lang_config(filepath) else {
            continue;
        };
        let chunks = chunk_file(&content, filepath, config);
        let symbols = crate::brain::outline::extract(filepath, &content);
        dirty.push((rel_str, mtime, size, chunks, symbols));
    }

    // Replace, don't append: a changed file must not keep its old chunks.
    let mut ghosts_removed = 0u64;
    let mut chunks_for_embedding: Vec<(String, String, String, String)> = Vec::new(); // id, label, content, source
    let mut per_file_chunks: Vec<(String, i64, i64, usize)> = Vec::new();

    for (rel_str, mtime, size, chunks, symbols) in &dirty {
        ghosts_removed += db.delete_nodes_for_source(&ws_key, rel_str).unwrap_or(0);
        for (label, chunk_content) in chunks {
            let content_hash = blake3_simple(chunk_content);
            let node_id = format!(
                "code:{}:{}:{}",
                ws_tag,
                rel_str.replace(':', "_"),
                &content_hash[..12]
            );
            chunks_for_embedding.push((
                node_id,
                label.clone(),
                chunk_content.clone(),
                rel_str.clone(),
            ));
        }
        // Symbol rows are small and carry no embedding: they exist so
        // `brain_search` can answer "where is X defined" with a precise line.
        for symbol in symbols {
            let slug: String = symbol
                .name
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .take(64)
                .collect();
            let id = format!(
                "symbol:{ws_tag}:{}:{}:{slug}",
                rel_str.replace(':', "_"),
                symbol.line
            );
            let node = crate::brain::db::Node::new(
                id,
                format!("{} ({}) · L{}", symbol.name, symbol.kind, symbol.line),
                symbol.signature.clone(),
                "symbol",
            )
            .with_source(rel_str)
            .with_workspace(&ws_key);
            if db.upsert_node(&node, None).is_err() {
                errors += 1;
            }
        }
        per_file_chunks.push((rel_str.clone(), *mtime, *size, chunks.len()));
    }

    let mut indexed = 0u64;
    let mut chunk_count = 0u64;
    let mut failed_sources: std::collections::HashSet<String> = std::collections::HashSet::new();

    for batch in chunks_for_embedding.chunks(32) {
        let texts: Vec<&str> = batch.iter().map(|(_, _, c, _)| c.as_str()).collect();
        let embeddings = match embedder.embed_batch(&texts) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("[brain] batch embedding failed: {err}");
                errors += batch.len() as u64;
                for (_, _, _, source) in batch {
                    failed_sources.insert(source.clone());
                }
                continue;
            }
        };
        for (i, (node_id, label, content, source)) in batch.iter().enumerate() {
            let node = Node::new(node_id.clone(), label.clone(), content.clone(), "code")
                .with_source(source)
                .with_workspace(&ws_key);
            if db
                .upsert_node(&node, embeddings.get(i).map(|v| v.as_slice()))
                .is_ok()
            {
                chunk_count += 1;
            } else {
                errors += 1;
            }
        }
    }

    // Bookkeeping per file, so the next run can skip it.
    for (rel_str, mtime, size, chunks) in &per_file_chunks {
        if failed_sources.contains(rel_str) {
            // Not marked as indexed: the next run must try again.
            continue;
        }
        indexed += 1;
        let _ = db.upsert_file_state(&ws_key, rel_str, *mtime, *size, *chunks as i64);
    }

    // Files that disappeared (deleted, renamed, moved out of the ignore rules)
    // lose their nodes and their bookkeeping row.
    let known = db.indexed_files_of(&ws_key).unwrap_or_default();
    let present: std::collections::HashSet<String> = files
        .iter()
        .map(|f| {
            f.strip_prefix(workspace)
                .unwrap_or(f)
                .to_string_lossy()
                .to_string()
        })
        .collect();
    for path in known {
        if !present.contains(&path) {
            ghosts_removed += db.delete_nodes_for_source(&ws_key, &path).unwrap_or(0);
            let _ = db.forget_file(&ws_key, &path);
        }
    }

    // Symbols are backfilled; the marker stops the next start from re-reading
    // every unchanged file again.
    db.kv_cache_put(&symbol_key, "1");

    Ok(IndexSummary {
        files_scanned: files.len() as u64,
        files_indexed: indexed,
        files_unchanged: unchanged,
        chunks_created: chunk_count,
        chunks_removed: ghosts_removed,
        dropped_unattributed,
        errors,
    })
}

pub(crate) fn blake3_simple(input: &str) -> String {
    // Simple 64-bit hash for dedup. On collision: overwrite — fine.
    // Fixed width (16 hex chars) on purpose: callers slice this string, and a
    // short value (e.g. hash 0x123) would otherwise panic on the slice.
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[derive(Debug, Clone)]
pub struct IndexSummary {
    pub files_scanned: u64,
    pub files_indexed: u64,
    /// Files whose mtime+size matched: not read, not embedded.
    pub files_unchanged: u64,
    pub chunks_created: u64,
    /// Chunks of changed or deleted files that were replaced/removed.
    pub chunks_removed: u64,
    /// Nodes dropped because they had no workspace (pre-scoping leftovers).
    pub dropped_unattributed: u64,
    pub errors: u64,
}

impl std::fmt::Display for IndexSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "brain: {} files scanned, {} indexed, {} unchanged, {} chunks written, {} old chunks removed, {} unattributed nodes dropped, {} errors",
            self.files_scanned,
            self.files_indexed,
            self.files_unchanged,
            self.chunks_created,
            self.chunks_removed,
            self.dropped_unattributed,
            self.errors
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_chunk_rust_file() {
        let content = "// small file\nfn main() {}\n";
        let path = Path::new("/test.rs");
        let cfg = get_lang_config(path).unwrap();
        let chunks = chunk_file(content, path, cfg);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_chunk_large_rust_file() {
        let mut lines = Vec::new();
        for i in 0..200 {
            lines.push(format!("fn func_{i}() {{ return {i}; }}"));
        }
        let content = lines.join("\n");
        let path = Path::new("/test.rs");
        let cfg = get_lang_config(path).unwrap();
        let chunks = chunk_file(&content, path, cfg);
        assert!(
            chunks.len() > 1,
            "should produce several chunks for 200 functions"
        );
    }

    #[test]
    fn test_collect_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("lib.py"), "def foo(): pass").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "nothing").unwrap(); // .txt is in the fallback config
        std::fs::write(dir.path().join("Cargo.lock"), "lock content").unwrap(); // ignored
        let files = collect_files(dir.path());
        let names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"main.rs".into()));
        assert!(names.contains(&"lib.py".into()));
        assert!(!names.contains(&"Cargo.lock".into()));
    }

    #[test]
    fn test_get_lang_config() {
        assert!(get_lang_config(Path::new("main.rs")).is_some());
        assert!(get_lang_config(Path::new("main.py")).is_some());
        assert!(get_lang_config(Path::new("main.ts")).is_some());
        assert!(get_lang_config(Path::new("main.jsx")).is_some());
        assert!(get_lang_config(Path::new("main.go")).is_some());
        assert!(get_lang_config(Path::new("main.cpp")).is_some());
        assert!(get_lang_config(Path::new("main.zig")).is_some());
        assert!(get_lang_config(Path::new("main.md")).is_some());
        assert!(get_lang_config(Path::new("Makefile")).is_none()); // without extension
    }

    #[test]
    fn test_blake3_simple_consistency() {
        let a = blake3_simple("hello");
        let b = blake3_simple("hello");
        assert_eq!(a, b);
        let c = blake3_simple("world");
        assert_ne!(a, c);
    }

    // ── Incremental indexing (step 3) ───────────────────────────────────────
    // Before: every start re-embedded whatever it could not recognise as
    // unchanged (a "skip" check per chunk with a DB roundtrip each), measured at
    // 65-70 s per start. Now: one stat per file, and only real changes are
    // embedded.

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StubEmbedder {
        calls: AtomicUsize,
    }

    impl StubEmbedder {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl ChunkEmbedder for StubEmbedder {
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(texts
                .iter()
                .map(|t| vec![t.len() as f32, 1.0, 0.0, 0.0])
                .collect())
        }
    }

    #[test]
    fn first_run_indexes_and_records_file_state() {
        let dir = tempdir().unwrap();
        let db = BrainDb::open(&dir.path().join("brain.db"), 4).unwrap();
        std::fs::write(dir.path().join("a.rs"), "pub fn alpha() {}\n").unwrap();
        let stub = StubEmbedder::new();

        let s = index_workspace(&db, &stub, dir.path()).unwrap();
        assert_eq!(s.files_indexed, 1, "{s}");
        assert_eq!(s.files_unchanged, 0, "{s}");
        assert!(s.chunks_created >= 1, "{s}");
        assert_eq!(
            stub.calls.load(Ordering::SeqCst),
            1,
            "one batch call expected"
        );

        let ws = dir
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            db.count_nodes(Some(&ws)).unwrap(),
            2,
            "one chunk + one symbol"
        );
        assert!(
            db.file_state(&ws, "a.rs").unwrap().is_some(),
            "file state missing"
        );
    }

    #[test]
    fn second_run_does_not_read_or_embed_unchanged_files() {
        let dir = tempdir().unwrap();
        let db = BrainDb::open(&dir.path().join("brain.db"), 4).unwrap();
        std::fs::write(dir.path().join("a.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "pub fn beta() {}\n").unwrap();
        let stub = StubEmbedder::new();
        let _ = index_workspace(&db, &stub, dir.path()).unwrap();
        let first_calls = stub.calls.load(Ordering::SeqCst);

        let s = index_workspace(&db, &stub, dir.path()).unwrap();
        assert_eq!(s.files_indexed, 0, "nothing to index on a warm run: {s}");
        assert_eq!(s.files_unchanged, 2, "{s}");
        assert_eq!(s.chunks_created, 0, "{s}");
        assert_eq!(
            stub.calls.load(Ordering::SeqCst),
            first_calls,
            "a warm run must not embed anything"
        );
    }

    #[test]
    fn changed_file_is_reindexed_and_old_chunks_do_not_survive() {
        let dir = tempdir().unwrap();
        let db = BrainDb::open(&dir.path().join("brain.db"), 4).unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "pub fn alpha_one() {}\n").unwrap();
        let stub = StubEmbedder::new();
        let _ = index_workspace(&db, &stub, dir.path()).unwrap();

        // Rewrite the file (different length + mtime) and re-index.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&file, "pub fn beta_two() { let x = 1; }\n").unwrap();
        let s = index_workspace(&db, &stub, dir.path()).unwrap();
        assert_eq!(s.files_indexed, 1, "{s}");
        assert!(
            s.chunks_removed >= 1,
            "old chunk should have been removed: {s}"
        );

        let ws = dir
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let labels: Vec<String> = db
            .search(&crate::brain::db::SearchQuery {
                text: "alpha_one",
                embedding: None,
                kind: None,
                workspace: Some(&ws),
                top_k: 10,
                min_score: 0.0,
            })
            .unwrap()
            .hits
            .iter()
            .map(|h| h.node.label.clone())
            .collect();
        assert!(
            labels.iter().all(|l| !l.contains("alpha_one")),
            "ghost chunk survived: {labels:?}"
        );
    }

    #[test]
    fn deleted_file_loses_its_nodes() {
        let dir = tempdir().unwrap();
        let db = BrainDb::open(&dir.path().join("brain.db"), 4).unwrap();
        let file = dir.path().join("gone.rs");
        std::fs::write(&file, "pub fn gone() {}\n").unwrap();
        let stub = StubEmbedder::new();
        let _ = index_workspace(&db, &stub, dir.path()).unwrap();
        let ws = dir
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            db.count_nodes(Some(&ws)).unwrap(),
            2,
            "one chunk + one symbol"
        );

        std::fs::remove_file(&file).unwrap();
        let s = index_workspace(&db, &stub, dir.path()).unwrap();
        assert_eq!(
            db.count_nodes(Some(&ws)).unwrap(),
            0,
            "nodes of a deleted file must go"
        );
        assert!(s.chunks_removed >= 1, "{s}");
        assert!(
            db.file_state(&ws, "gone.rs").unwrap().is_none(),
            "bookkeeping must go too"
        );
    }

    #[test]
    fn another_workspace_keeps_its_nodes() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        let db = BrainDb::open(&a.path().join("brain.db"), 4).unwrap();
        std::fs::write(a.path().join("a.rs"), "pub fn from_a() {}\n").unwrap();
        std::fs::write(b.path().join("b.rs"), "pub fn from_b() {}\n").unwrap();
        let stub = StubEmbedder::new();
        let _ = index_workspace(&db, &stub, a.path()).unwrap();
        let _ = index_workspace(&db, &stub, b.path()).unwrap();

        let wa = a
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let wb = b
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            db.count_nodes(Some(&wa)).unwrap(),
            2,
            "workspace A lost its nodes"
        );
        assert_eq!(
            db.count_nodes(Some(&wb)).unwrap(),
            2,
            "workspace B not indexed"
        );
        // Indexing B must not have touched A's bookkeeping.
        assert!(db.file_state(&wa, "a.rs").unwrap().is_some());
    }
}
